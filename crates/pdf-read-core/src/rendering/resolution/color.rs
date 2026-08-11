//! Colour-resolution stage.
//!
//! This is the stage where capabilities that previously could not reach the
//! renderer are wired in:
//!
//! - **PostScript Type 4 calculator** tint transforms ([`crate::functions`]).
//!   Resolves `Separation` and `DeviceN` colour spaces whose `tintTransform`
//!   is a Type-4 function — the case the inline match arm at
//!   `page_renderer.rs:629-693` falls back to `1.0 - tint` for.
//! - **Type 2 exponential interpolation** tint transforms. Spec
//!   ISO 32000-1:2008 §7.10.3. The existing inline match arm handles this
//!   for `DeviceCMYK` alternate spaces only; the resolver handles `DeviceRGB`
//!   and `DeviceGray` alternates as well.
//! - **ICCBased** colour spaces. The resolver delegates to the
//!   [`crate::color::Transform`] CMM when the `icc` feature is on and falls
//!   back to the process-ink conversion otherwise. This is the same
//!   path image extraction uses, so we re-use [`crate::color`] rather than
//!   carrying a second copy of the conversion code.
//! - **Indexed** colour spaces. The resolver follows the index into the base
//!   space; for now we handle DeviceGray / DeviceRGB / DeviceCMYK base spaces
//!   and fall back to grayscale otherwise (matching the existing renderer).
//!
//! The output is a [`ResolvedColor::Rgba`] for composite consumers; a
//! follow-up branch will add the `Cmyk` and `PerChannel` variants behind the
//! same resolver entry point so separation backends share the same call.

use crate::error::Result;
use crate::object::Object;

use super::context::ResolutionContext;
use super::intent::{DeviceColor, LogicalColor};
use super::resolved::ResolvedColor;

mod functions;

pub(crate) use functions::cmyk_to_rgb_via_intent;
use functions::{
    array_to_pairs, cmyk_to_rgb, device_to_rgba, evaluate_tint_function, evaluate_type0_sampled,
    evaluate_type2, evaluate_type3_stitching, evaluate_type4, first_as_gray, four_as_cmyk,
    four_as_cmyk_native, object_to_f32, object_to_f64, resolve_device_alias, three_as_rgb,
};

/// Colour-resolution stage.
///
/// Stateless — the resolver is purely a function of `(LogicalColor,
/// ResolutionContext, gs.fill_alpha-or-stroke_alpha)`. The struct exists so
/// the pipeline can grow per-instance state later (e.g. a cache of compiled
/// Type-4 [`crate::functions::Program`] keyed by stream object id) without
/// changing the call surface.
pub(crate) struct ColorResolver;

impl ColorResolver {
    pub(crate) const fn new() -> Self {
        Self
    }

    /// Resolve `color` into an RGBA value the composite backend can paint.
    ///
    /// `alpha` is the pre-computed straight alpha from the graphics state
    /// (i.e. `gs.fill_alpha` for fill intents, `gs.stroke_alpha` for stroke
    /// intents). Folding it in here keeps backends simple.
    pub(crate) fn resolve(
        &self,
        color: &LogicalColor,
        ctx: &ResolutionContext,
        alpha: f32,
    ) -> Result<ResolvedColor> {
        match color {
            LogicalColor::Device(dev) => {
                // ISO 32000-1:2008 §8.6.5.6: when the page declares a
                // /DefaultGray, /DefaultRGB, or /DefaultCMYK entry in
                // its /Resources /ColorSpace dict, any bare device-family
                // paint operator (the canonical `g`/`rg`/`k`/`K` and
                // their stroking siblings) MUST be interpreted as if it
                // had named the override colour space instead of the
                // device family. The override therefore takes
                // precedence over the document /OutputIntents profile
                // for bare device paint — OutputIntent is only the
                // fallback default when no override has been declared.
                if let Some(resolved) = self.resolve_device_default_override(*dev, ctx, alpha)? {
                    return Ok(resolved);
                }
                Ok(device_to_rgba(*dev, alpha))
            }
            LogicalColor::Spaced { space, components } => {
                self.resolve_spaced(space, components, ctx, alpha)
            }
        }
    }

    /// §8.6.5.6 dispatch for bare device-family paint. Returns `Some`
    /// when the active page has declared a matching `/Default<Family>`
    /// override AND that override resolves successfully; otherwise
    /// returns `None` so the caller emits the device-family default.
    ///
    /// The override is resolved by recursively calling `resolve_spaced`
    /// on the override object with the original paint components. That
    /// reuses the existing colour-space machinery (ICCBased N=3/N=4,
    /// Separation, DeviceN, …) so a `/DefaultCMYK [/ICCBased ...]`
    /// override goes through the embedded-ICC path, picks up the
    /// per-page transform cache via `ctx.icc_transform_cache`, and
    /// emits `ResolvedColor::IccCmyk` exactly as for an explicit
    /// `[/ICCBased N=4]` colour space paint.
    ///
    /// Precedence note: this fires BEFORE the OutputIntent-aware CMYK
    /// projection at `cmyk_to_rgb_via_intent` because the override is
    /// the page's declared colour space and OutputIntent only fills
    /// in for the device family when no override is present.
    fn resolve_device_default_override(
        &self,
        dev: DeviceColor,
        ctx: &ResolutionContext,
        alpha: f32,
    ) -> Result<Option<ResolvedColor>> {
        let (override_obj, components): (Option<&Object>, smallvec::SmallVec<[f32; 4]>) = match dev
        {
            DeviceColor::Gray(g) => (ctx.default_gray, smallvec::smallvec![g]),
            DeviceColor::Rgb(r, g, b) => (ctx.default_rgb, smallvec::smallvec![r, g, b]),
            DeviceColor::Cmyk(c, m, y, k) => (ctx.default_cmyk, smallvec::smallvec![c, m, y, k]),
        };
        let Some(space) = override_obj else {
            return Ok(None);
        };

        // §8.6.5.6 requires the override entry to be a colour space:
        // either a Name (device-family alias such as `/DeviceCMYK`,
        // `/CalGray`) or an Array (`[/ICCBased ...]`, `[/Separation
        // ...]`, etc.). A malformed entry (string, integer, bool,
        // dictionary…) is structurally indistinguishable from the
        // entry being absent — honouring it would silently
        // mis-render through `resolve_spaced`'s `first_as_gray`
        // catch-all (a quarter-tint CMYK paint coming out as 25%
        // gray is worse than the spec-fallback / OutputIntent
        // render). Return None so the caller falls through to the
        // device-family path (`device_to_rgba`), which routes CMYK
        // through `cmyk_to_rgb_via_intent` and so consults
        // `/OutputIntents` when present, or the process-ink conversion
        // when not.
        if space.as_name().is_none() && space.as_array().is_none() {
            return Ok(None);
        }

        // The override resolves via the same colour-space pipeline
        // as an explicit `cs <space>` paint — that's the whole point
        // of §8.6.5.6: the override colour space stands in for the
        // device family. If the override object is just another Name
        // (e.g. `/DefaultCMYK /DeviceCMYK`, an identity declaration),
        // resolve_spaced's Name arm folds back to the device-family
        // default — returning Some is still correct because we've
        // honoured the override; it just produces the same value as
        // the no-override path.
        Ok(Some(self.resolve_spaced(space, &components, ctx, alpha)?))
    }

    fn resolve_spaced(
        &self,
        space: &Object,
        components: &[f32],
        ctx: &ResolutionContext,
        alpha: f32,
    ) -> Result<ResolvedColor> {
        // A `Name` here means a device family — the operator dispatcher
        // already folded those into LogicalColor::Device for the canonical
        // `g`/`rg`/`k`/`K` operators, but `SCN` against a Device* alias
        // still reaches us this way.
        if let Some(name) = space.as_name() {
            return Ok(resolve_device_alias(name, components, alpha));
        }

        let Some(arr) = space.as_array() else {
            // Unknown space shape — fall back to first-component-as-gray,
            // matching the existing inline behaviour at
            // `page_renderer.rs:709-712`.
            return Ok(first_as_gray(components, alpha));
        };

        let Some(type_name) = arr.first().and_then(|o| o.as_name()) else {
            return Ok(first_as_gray(components, alpha));
        };

        match type_name {
            "DeviceGray" | "G" | "CalGray" => Ok(first_as_gray(components, alpha)),
            "DeviceRGB" | "RGB" | "CalRGB" => Ok(three_as_rgb(components, alpha)),
            "DeviceCMYK" | "CMYK" => Ok(four_as_cmyk_native(components, alpha)),
            "ICCBased" => self.resolve_iccbased(arr, components, ctx, alpha),
            "Separation" | "DeviceN" => {
                self.resolve_separation_or_devicen(arr, components, ctx, alpha)
            }
            "Indexed" => self.resolve_indexed(arr, components, ctx, alpha),
            _ => Ok(first_as_gray(components, alpha)),
        }
    }

    fn resolve_iccbased(
        &self,
        arr: &[Object],
        components: &[f32],
        ctx: &ResolutionContext,
        alpha: f32,
    ) -> Result<ResolvedColor> {
        // ICCBased array shape: [/ICCBased <stream-ref>]. The stream dict
        // carries /N indicating the input component count.
        let Some(stream_obj) = arr.get(1) else {
            return Ok(first_as_gray(components, alpha));
        };
        let resolved_stream = match ctx.doc.resolve_object(stream_obj) {
            Ok(o) => o,
            Err(_) => return Ok(first_as_gray(components, alpha)),
        };
        let Some(dict) = resolved_stream.as_dict() else {
            return Ok(first_as_gray(components, alpha));
        };
        let n = dict.get("N").and_then(|o| o.as_integer()).unwrap_or(3);

        // §8.6.5.5 precedence: an ICCBased colour space carries its own
        // conversion source. The embedded profile wins over the document
        // /OutputIntents profile when CMYK→RGB is requested. Decode the
        // stream, parse the bytes through IccProfile::parse (which
        // cross-checks the dict's /N against the ICC header signature),
        // and compile a qcms Transform against the active rendering
        // intent. On any failure (no `icc` feature, decode error,
        // mismatched header, qcms refusal) we fall through to the
        // device-family path — that path emits ResolvedColor::Cmyk for
        // N=4, which the composite projection then converts through
        // ctx.output_intent_cmyk: the document OutputIntent becomes the
        // default when the embedded profile can't actually drive a CMM.
        //
        // We emit the dual-payload `IccCmyk` variant so the per-plate
        // router still sees the four channel decomposition. The composite
        // backend reads the pre-computed RGB; the separation backend
        // reads the original CMYK quadruple. The ICC conversion is a
        // composite-surface concern — the plates ARE the press-target
        // ink coverage, so dropping the CMYK channel values for a
        // monolithic Rgba would zero out every plate.
        #[cfg(feature = "icc-qcms")]
        if n == 4 && components.len() >= 4 {
            if let Ok(bytes) = resolved_stream.decode_stream_data() {
                if let Some(profile) = crate::color::IccProfile::parse(bytes, 4) {
                    let profile = std::sync::Arc::new(profile);
                    // Per-page transform cache keyed on profile content
                    // hash + intent (see IccTransformCache). The
                    // embedded /ICCBased profile is parsed afresh on
                    // every paint operator (the decode + parse happens
                    // above), but the qcms CMM is the heavy bit and
                    // gets reused across paints whose ICCBased stream
                    // hashes identically. Unit tests skip the cache
                    // (ctx.icc_transform_cache is None) and pay the
                    // per-call build cost.
                    let transform: std::sync::Arc<crate::color::Transform> =
                        if let Some(cache) = ctx.icc_transform_cache {
                            cache.get_or_build(&profile, ctx.rendering_intent)
                        } else {
                            std::sync::Arc::new(crate::color::Transform::new_srgb_target(
                                std::sync::Arc::clone(&profile),
                                ctx.rendering_intent,
                            ))
                        };
                    if transform.has_cmm() {
                        let c = components[0].clamp(0.0, 1.0);
                        let m = components[1].clamp(0.0, 1.0);
                        let y = components[2].clamp(0.0, 1.0);
                        let k = components[3].clamp(0.0, 1.0);
                        let c_u8 = (c * 255.0).round() as u8;
                        let m_u8 = (m * 255.0).round() as u8;
                        let y_u8 = (y * 255.0).round() as u8;
                        let k_u8 = (k * 255.0).round() as u8;
                        let rgb = transform.convert_cmyk_pixel(c_u8, m_u8, y_u8, k_u8);
                        return Ok(ResolvedColor::IccCmyk {
                            r: rgb[0] as f32 / 255.0,
                            g: rgb[1] as f32 / 255.0,
                            b: rgb[2] as f32 / 255.0,
                            c,
                            m,
                            y,
                            k,
                            a: alpha,
                        });
                    }
                }
            }
        }

        // ICCBased N=3 — RGB source profile. The embedded profile
        // drives the conversion (§8.6.5.5); the §10.3.5 fallback only
        // fires when qcms refuses to compile the profile. This branch
        // is also the path the §8.6.5.6 /DefaultRGB override consumes:
        // declaring `/DefaultRGB [/ICCBased <N=3 stream>]` and painting
        // bare /DeviceRGB sends the three components through this arm.
        //
        // No per-plate routing complication here — RGB never lands on
        // CMYK plates — so we emit ResolvedColor::Rgba directly. The
        // per-page transform cache (originally introduced for CMYK,
        // but n_components-agnostic at the key level — see
        // `IccTransformCache` docstring) is consulted here too: an
        // /ICCBased N=3 profile used by a /DefaultRGB override gets
        // hit by every bare /DeviceRGB paint on the page, so caching
        // the compiled qcms transform pays back for the same reason
        // the CMYK arm above does.
        #[cfg(feature = "icc-qcms")]
        if n == 3 && components.len() >= 3 {
            if let Ok(bytes) = resolved_stream.decode_stream_data() {
                if let Some(profile) = crate::color::IccProfile::parse(bytes, 3) {
                    let profile = std::sync::Arc::new(profile);
                    let transform: std::sync::Arc<crate::color::Transform> =
                        if let Some(cache) = ctx.icc_transform_cache {
                            cache.get_or_build(&profile, ctx.rendering_intent)
                        } else {
                            std::sync::Arc::new(crate::color::Transform::new_srgb_target(
                                std::sync::Arc::clone(&profile),
                                ctx.rendering_intent,
                            ))
                        };
                    if transform.has_cmm() {
                        let r = components[0].clamp(0.0, 1.0);
                        let g = components[1].clamp(0.0, 1.0);
                        let b = components[2].clamp(0.0, 1.0);
                        let r_u8 = (r * 255.0).round() as u8;
                        let g_u8 = (g * 255.0).round() as u8;
                        let b_u8 = (b * 255.0).round() as u8;
                        let rgb = transform.convert_rgb_buffer(&[r_u8, g_u8, b_u8]);
                        if rgb.len() >= 3 {
                            return Ok(ResolvedColor::Rgba {
                                r: rgb[0] as f32 / 255.0,
                                g: rgb[1] as f32 / 255.0,
                                b: rgb[2] as f32 / 255.0,
                                a: alpha,
                            });
                        }
                    }
                }
            }
        }

        // ICCBased N=1 — Gray source profile. The embedded profile
        // drives the conversion (§8.6.5.5) and is the path
        // /DefaultGray [/ICCBased <N=1 TRC stream>] consumes for bare
        // /DeviceGray paint. qcms 0.3.0 reads Gray ICC profiles via
        // the `kTRC` (gray Tone Reproduction Curve) tag —
        // `iccread.rs:1712-1714` — and runs a dedicated
        // gray-to-RGB transform path at `transform.rs:437-475`. The
        // input is one byte, the output is three RGB bytes; we read
        // the first three of `convert_gray_buffer`'s output.
        //
        // No per-plate routing complication — a Gray override emits
        // a single ink and lands on the K plate via the InkRouter's
        // gray-as-K handling; the composite RGB is what consumers
        // see, so ResolvedColor::Rgba is the right variant. The
        // per-page transform cache is consulted exactly as for N=3
        // and N=4 — the key is (profile.content_hash(), intent), no
        // n_components in the key, so the same cache amortises Gray
        // ICC alongside RGB and CMYK.
        #[cfg(feature = "icc-qcms")]
        if n == 1 && !components.is_empty() {
            if let Ok(bytes) = resolved_stream.decode_stream_data() {
                if let Some(profile) = crate::color::IccProfile::parse(bytes, 1) {
                    let profile = std::sync::Arc::new(profile);
                    let transform: std::sync::Arc<crate::color::Transform> =
                        if let Some(cache) = ctx.icc_transform_cache {
                            cache.get_or_build(&profile, ctx.rendering_intent)
                        } else {
                            std::sync::Arc::new(crate::color::Transform::new_srgb_target(
                                std::sync::Arc::clone(&profile),
                                ctx.rendering_intent,
                            ))
                        };
                    if transform.has_cmm() {
                        let g = components[0].clamp(0.0, 1.0);
                        let g_u8 = (g * 255.0).round() as u8;
                        let rgb = transform.convert_gray_buffer(&[g_u8]);
                        if rgb.len() >= 3 {
                            return Ok(ResolvedColor::Rgba {
                                r: rgb[0] as f32 / 255.0,
                                g: rgb[1] as f32 / 255.0,
                                b: rgb[2] as f32 / 255.0,
                                a: alpha,
                            });
                        }
                    }
                }
            }
        }

        // No usable embedded profile — fall through to the device-family
        // hint. For N=4 this emits ResolvedColor::Cmyk so per-plate
        // backends still see the channel decomposition, and the
        // composite projection routes through ctx.output_intent_cmyk
        // (which is the spec default when no embedded ICC is available).
        match n {
            1 if !components.is_empty() => Ok(first_as_gray(components, alpha)),
            3 if components.len() >= 3 => Ok(three_as_rgb(components, alpha)),
            4 if components.len() >= 4 => Ok(four_as_cmyk_native(components, alpha)),
            _ => Ok(first_as_gray(components, alpha)),
        }
    }

    /// Resolve `Separation` and `DeviceN` colour spaces by evaluating the
    /// tint transform.
    ///
    /// Array shape: `[/Separation name altCS tintTransform]` or
    /// `[/DeviceN names altCS tintTransform attrs?]`. The tint transform is
    /// a PDF function dict whose `FunctionType` selects:
    ///
    /// - **Type 0** (sampled): N-dimensional multilinear interpolation over
    ///   the sample grid (see [`evaluate_type0_sampled`]) — handles both the
    ///   common 1-input Separation shape and genuinely multi-channel
    ///   DeviceN tint transforms.
    /// - **Type 2** (exponential): closed-form interpolation between `/C0`
    ///   and `/C1` with exponent `/N`. The existing inline path only handles
    ///   `N=1` against `DeviceCMYK` altCS; we generalise to any `N` and to
    ///   `DeviceRGB`/`DeviceGray` altCS as well.
    /// - **Type 3** (stitching): single-input by spec; picks the matching
    ///   sub-function and delegates.
    /// - **Type 4** (calculator): evaluated via [`crate::functions::Program`].
    fn resolve_separation_or_devicen(
        &self,
        arr: &[Object],
        components: &[f32],
        ctx: &ResolutionContext,
        alpha: f32,
    ) -> Result<ResolvedColor> {
        if components.is_empty() {
            return Ok(ResolvedColor::Rgba {
                r: 0.0,
                g: 0.0,
                b: 0.0,
                a: alpha,
            });
        }

        // §8.6.6.3 reserved name: `/None` produces no visible output.
        // For composite output we emit a fully-transparent RGBA — the
        // splice carries it through as a no-op. The per-plate route
        // sees `InkSelector::None` via the OverprintPlan and skips
        // every plate regardless of this colour value.
        let type_name = arr.first().and_then(|o| o.as_name());
        if matches!(type_name, Some("Separation"))
            && arr.get(1).and_then(|o| o.as_name()) == Some("None")
        {
            return Ok(ResolvedColor::Rgba {
                r: 0.0,
                g: 0.0,
                b: 0.0,
                a: 0.0,
            });
        }

        // Determine alternate colour space and tint-transform function.
        // Separation: [/Separation name altCS tintTransform]
        // DeviceN: [/DeviceN names altCS tintTransform attrs?]
        //
        // When the array is malformed (no altCS or no tintTransform), or
        // the function dict is missing / unrecognised, we fall back to
        // `g = 1.0 - tint`. This mirrors the long-standing inline `scn`
        // and `SCN` behaviour: callers exist that rely on it as a
        // "darker = more ink" heuristic for spot inks that never wired
        // up a proper tint transform. Off-vs-on toggle parity holds
        // until the broader §8.6.6.4 fix lands.
        let invert_tint_fallback = |components: &[f32], alpha: f32| -> ResolvedColor {
            let t = components.first().copied().unwrap_or(0.0);
            let g = (1.0 - t).clamp(0.0, 1.0);
            ResolvedColor::Rgba {
                r: g,
                g,
                b: g,
                a: alpha,
            }
        };

        let alt_cs_obj = match arr.get(2) {
            Some(o) => o,
            None => return Ok(invert_tint_fallback(components, alpha)),
        };
        let func_obj = match arr.get(3) {
            Some(o) => o,
            None => return Ok(invert_tint_fallback(components, alpha)),
        };

        // The alternate colour space may itself be an indirect reference
        // (e.g. `[/Separation /Spot 6 0 R 5 0 R]`) - resolve it before
        // inspecting its shape, mirroring how `func_obj` is resolved just
        // below. Otherwise `.as_name()` returns `None` on the unresolved
        // `Reference` and the compound-array check also fails, so control
        // falls through to the `first_as_gray` fallback for a colour space
        // that is really `/DeviceRGB` or an indirect `[/ICCBased ...]`.
        let alt_cs_resolved = match ctx.doc.resolve_object(alt_cs_obj) {
            Ok(o) => o,
            Err(_) => return Ok(invert_tint_fallback(components, alpha)),
        };

        let func_resolved = match ctx.doc.resolve_object(func_obj) {
            Ok(o) => o,
            Err(_) => return Ok(invert_tint_fallback(components, alpha)),
        };
        // FunctionType may be in the dict directly (Type 2/3) or in the
        // stream dict (Type 0/4). `as_dict` handles both.
        let Some(func_dict) = func_resolved.as_dict() else {
            return Ok(invert_tint_fallback(components, alpha));
        };
        let func_type = func_dict
            .get("FunctionType")
            .and_then(|o| o.as_integer())
            .unwrap_or(-1);

        let alt_cs_name = alt_cs_resolved.as_name();

        let altspace_values: Vec<f32> = match func_type {
            // Type 0 honours every input component (a multi-channel
            // DeviceN's sampled tint transform is genuinely N-dimensional);
            // Type 3 stitching is single-input by spec and only consults
            // the first component internally.
            0 | 3 => match evaluate_tint_function(ctx, &func_resolved, components, 0) {
                Some(v) => v,
                // Outside the supported envelope (exotic bit depths,
                // malformed Domain, over-deep nesting): keep the
                // long-standing fallback rather than guess.
                None => return Ok(invert_tint_fallback(components, alpha)),
            },
            2 => evaluate_type2(func_dict, components[0]),
            4 => evaluate_type4(&func_resolved, components)?,
            _ => return Ok(invert_tint_fallback(components, alpha)),
        };

        // Project the alternate-space values through their colour space.
        // The per-plate routing (which named plate gets the tint, what
        // happens to other plates) is determined by the source colour
        // space — Separation /Pantone-185 paints the Pantone-185 plate,
        // not the C/M/Y/K plates. That routing decision lives on the
        // OverprintPlan's `participating`, stamped by the pipeline
        // composer (see `apply_inks_selector_override`).
        //
        // The composite-side colour resolution is the alternate-space
        // value projected to RGBA — that's what the alternate is for
        // per §8.6.6.3 (composite-only fallback). Emit ResolvedColor::Rgba
        // here so the composite backend gets the right colour without
        // accidentally feeding the alternate's CMYK decomposition into
        // the per-plate path.
        match alt_cs_name {
            Some("DeviceCMYK") | Some("CMYK") if altspace_values.len() >= 4 => {
                Ok(four_as_cmyk(&altspace_values, alpha, ctx))
            }
            Some("DeviceRGB") | Some("RGB") if altspace_values.len() >= 3 => {
                Ok(three_as_rgb(&altspace_values, alpha))
            }
            Some("DeviceGray") | Some("G") if !altspace_values.is_empty() => {
                Ok(first_as_gray(&altspace_values, alpha))
            }
            _ => {
                // Compound alternate space (e.g. ICCBased). We synthesise a
                // logical Spaced colour and recurse — this lets a
                // Separation with an ICC alternate route through the ICC
                // branch correctly.
                if let Object::Array(_) = alt_cs_resolved {
                    self.resolve_spaced(&alt_cs_resolved, &altspace_values, ctx, alpha)
                } else {
                    Ok(first_as_gray(&altspace_values, alpha))
                }
            }
        }
    }

    fn resolve_indexed(
        &self,
        arr: &[Object],
        components: &[f32],
        _ctx: &ResolutionContext,
        alpha: f32,
    ) -> Result<ResolvedColor> {
        // Indexed: [/Indexed base hival lookup]. The component is the
        // palette index, scaled 0..255 inside the renderer's existing
        // inline path. We replicate that fallback (gray = index/255) since
        // the full lookup path requires palette-stream decoding the pilot
        // operator doesn't need yet. Image extraction handles indexed
        // images through a richer path in `src/extractors/images.rs`.
        let _ = arr;
        if components.is_empty() {
            return Ok(ResolvedColor::Rgba {
                r: 0.0,
                g: 0.0,
                b: 0.0,
                a: alpha,
            });
        }
        let g = (components[0] / 255.0).clamp(0.0, 1.0);
        Ok(ResolvedColor::Rgba {
            r: g,
            g,
            b: g,
            a: alpha,
        })
    }
}
#[cfg(test)]
mod tests;
