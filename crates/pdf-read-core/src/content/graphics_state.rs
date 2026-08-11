//! Graphics state management for content stream execution.
//!
//! This module provides the graphics state machine that tracks transformations,
//! text positioning, colors, and other parameters as operators are executed.

use crate::geometry::Point;

/// A 2D transformation matrix.
///
/// PDF uses matrices of the form:
/// ```text
/// [ a  b  0 ]
/// [ c  d  0 ]
/// [ e  f  1 ]
/// ```
///
/// Where (a,b,c,d) define scaling/rotation/skewing and (e,f) define translation.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Matrix {
    /// Horizontal scaling component
    pub a: f32,
    /// Rotation/skew component
    pub b: f32,
    /// Rotation/skew component
    pub c: f32,
    /// Vertical scaling component
    pub d: f32,
    /// Horizontal translation
    pub e: f32,
    /// Vertical translation
    pub f: f32,
}

impl Matrix {
    /// Create an identity matrix.
    ///
    /// The identity matrix represents no transformation.
    ///
    /// # Examples
    ///
    /// ```
    /// use pdf_oxide::content::Matrix;
    ///
    /// let m = Matrix::identity();
    /// assert_eq!(m.a, 1.0);
    /// assert_eq!(m.d, 1.0);
    /// assert_eq!(m.e, 0.0);
    /// assert_eq!(m.f, 0.0);
    /// ```
    pub fn identity() -> Self {
        Self {
            a: 1.0,
            b: 0.0,
            c: 0.0,
            d: 1.0,
            e: 0.0,
            f: 0.0,
        }
    }

    /// Whether this matrix is the identity (applies no transform).
    ///
    /// Callers that cache CTM-transformed coordinates use this to decide
    /// whether the cache is safe to reuse across invocations — non-identity
    /// matrices mean coordinates differ per call.
    pub fn is_identity(&self) -> bool {
        self.a == 1.0
            && self.b == 0.0
            && self.c == 0.0
            && self.d == 1.0
            && self.e == 0.0
            && self.f == 0.0
    }

    /// Effective scale this matrix applies to a stroked line width.
    ///
    /// Per ISO 32000-1:2008 §8.4.3.2 the line width is expressed in user
    /// space and transformed by the CTM like all other geometry. A
    /// non-uniform matrix has no single width scale, so this returns the
    /// standard `sqrt(|det|)` (area-preserving) approximation used by PDF
    /// rasterizers; it is exact for uniform scaling and rotation, and `1.0`
    /// for translation-only matrices.
    ///
    /// # Examples
    ///
    /// ```
    /// use pdf_oxide::content::Matrix;
    ///
    /// assert_eq!(Matrix::identity().stroke_scale(), 1.0);
    /// assert_eq!(Matrix::scaling(3.0, 3.0).stroke_scale(), 3.0);
    /// assert_eq!(Matrix::translation(10.0, 20.0).stroke_scale(), 1.0);
    /// ```
    pub fn stroke_scale(&self) -> f32 {
        (self.a * self.d - self.b * self.c).abs().sqrt()
    }

    /// Create a translation matrix.
    ///
    /// # Examples
    ///
    /// ```
    /// use pdf_oxide::content::Matrix;
    ///
    /// let m = Matrix::translation(10.0, 20.0);
    /// assert_eq!(m.e, 10.0);
    /// assert_eq!(m.f, 20.0);
    /// ```
    pub fn translation(tx: f32, ty: f32) -> Self {
        Self {
            a: 1.0,
            b: 0.0,
            c: 0.0,
            d: 1.0,
            e: tx,
            f: ty,
        }
    }

    /// Create a scaling matrix.
    ///
    /// # Examples
    ///
    /// ```
    /// use pdf_oxide::content::Matrix;
    ///
    /// let m = Matrix::scaling(2.0, 3.0);
    /// assert_eq!(m.a, 2.0);
    /// assert_eq!(m.d, 3.0);
    /// ```
    pub fn scaling(sx: f32, sy: f32) -> Self {
        Self {
            a: sx,
            b: 0.0,
            c: 0.0,
            d: sy,
            e: 0.0,
            f: 0.0,
        }
    }

    /// Multiply this matrix with another matrix.
    ///
    /// Matrix multiplication is not commutative: A * B ≠ B * A.
    /// The result represents first applying `other`, then applying `self`.
    ///
    /// # Examples
    ///
    /// ```
    /// use pdf_oxide::content::Matrix;
    ///
    /// let m1 = Matrix::translation(10.0, 0.0);
    /// let m2 = Matrix::scaling(2.0, 2.0);
    /// let result = m1.multiply(&m2);
    /// ```
    pub fn multiply(&self, other: &Matrix) -> Matrix {
        Matrix {
            a: self.a * other.a + self.b * other.c,
            b: self.a * other.b + self.b * other.d,
            c: self.c * other.a + self.d * other.c,
            d: self.c * other.b + self.d * other.d,
            e: self.e * other.a + self.f * other.c + other.e,
            f: self.e * other.b + self.f * other.d + other.f,
        }
    }

    /// Transform a point using this matrix.
    ///
    /// Applies the transformation defined by this matrix to the given point.
    ///
    /// # Examples
    ///
    /// ```
    /// use pdf_oxide::content::Matrix;
    /// use pdf_oxide::geometry::Point;
    ///
    /// let m = Matrix::translation(10.0, 20.0);
    /// let p = m.transform_point(5.0, 10.0);
    /// assert_eq!(p.x, 15.0);
    /// assert_eq!(p.y, 30.0);
    /// ```
    pub fn transform_point(&self, x: f32, y: f32) -> Point {
        Point {
            x: self.a * x + self.c * y + self.e,
            y: self.b * x + self.d * y + self.f,
        }
    }

    /// Get the determinant of this matrix.
    ///
    /// The determinant indicates if the matrix is invertible (non-zero)
    /// and the scaling factor it applies to areas.
    pub fn determinant(&self) -> f32 {
        self.a * self.d - self.b * self.c
    }

    /// Check if this matrix is invertible.
    ///
    /// A matrix is invertible if its determinant is non-zero.
    pub fn is_invertible(&self) -> bool {
        self.determinant().abs() > f32::EPSILON
    }
}

impl Default for Matrix {
    fn default() -> Self {
        Self::identity()
    }
}

/// Graphics state parameters.
///
/// Tracks all parameters that affect how content is rendered, including
/// transformations, colors, line styles, and text state.
#[derive(Debug, Clone)]
pub struct GraphicsState {
    /// Current transformation matrix (maps user space to device space)
    pub ctm: Matrix,
    /// Text matrix (maps text space to user space)
    pub text_matrix: Matrix,
    /// Text line matrix (saved position at start of line)
    pub text_line_matrix: Matrix,

    // Text state parameters
    /// Character spacing (Tc)
    pub char_space: f32,
    /// Word spacing (Tw)
    pub word_space: f32,
    /// Horizontal scaling percentage (Tz)
    pub horizontal_scaling: f32,
    /// Text leading (TL)
    pub leading: f32,
    /// Current font name
    pub font_name: Option<String>,
    /// Current font size (Tf)
    pub font_size: f32,
    /// Text rise (Ts)
    pub text_rise: f32,
    /// Text rendering mode (Tr)
    pub render_mode: u8,
    /// Writing mode for the currently selected font (0 = horizontal, 1 = vertical).
    ///
    /// Cached here rather than dereferenced from the font on every Tj/TJ so
    /// the advance helpers in the extractor and rasterizer can branch on a
    /// single primitive field read. Refreshed whenever the operator loop
    /// processes a Tf (font selection) operator. Defaults to `0` so the
    /// horizontal fast path stays cold on every existing document and
    /// hot-path test that constructs a fresh state without calling Tf.
    pub text_wmode: u8,

    // Color parameters
    /// Fill color space name (DeviceRGB, DeviceCMYK, DeviceGray, etc.)
    pub fill_color_space: String,
    /// Stroke color space name (DeviceRGB, DeviceCMYK, DeviceGray, etc.)
    pub stroke_color_space: String,
    /// Fill color (RGB)
    pub fill_color_rgb: (f32, f32, f32),
    /// Stroke color (RGB)
    pub stroke_color_rgb: (f32, f32, f32),
    /// Fill color (CMYK) - optional, if CMYK color space is used
    pub fill_color_cmyk: Option<(f32, f32, f32, f32)>,
    /// Stroke color (CMYK) - optional, if CMYK color space is used
    pub stroke_color_cmyk: Option<(f32, f32, f32, f32)>,
    /// Raw fill-colour components from the most recent `sc`/`scn`/`g`/`rg`/`k`
    /// operator. The renderer's inline match arms always evaluate these to
    /// RGB eagerly; this field retains the originals so the renderer's
    /// resolution pipeline can re-evaluate them through a richer path
    /// (e.g. PostScript Type 4 tint transforms) when the pilot operator
    /// is routed through it. The pipeline itself lives under the
    /// `rendering` feature so it cannot be linked from here without
    /// breaking no-default-features doc builds.
    ///
    /// Empty means "no explicit fill colour set yet" (the implicit PDF
    /// default is DeviceGray black, which the dispatcher records by setting
    /// `fill_color_rgb = (0,0,0)`).
    pub fill_color_components: smallvec::SmallVec<[f32; 8]>,
    /// Raw stroke-colour components from the most recent `SC`/`SCN`/`G`/`RG`/`K`
    /// operator. See [`Self::fill_color_components`] for rationale.
    pub stroke_color_components: smallvec::SmallVec<[f32; 8]>,

    // Line parameters (for completeness, though mainly used for graphics)
    /// Line width
    pub line_width: f32,
    /// Line dash pattern ([on1, off1, on2, off2, ...], phase)
    /// Empty array means solid line
    pub dash_pattern: (Vec<f32>, f32),
    /// Line cap style (J): 0=butt cap, 1=round cap, 2=projecting square cap
    pub line_cap: u8,
    /// Line join style (j): 0=miter join, 1=round join, 2=bevel join
    pub line_join: u8,
    /// Miter limit (M): ratio of miter length to line width
    pub miter_limit: f32,
    /// Rendering intent (ri): color rendering intent for images and graphics
    pub rendering_intent: String,
    /// Flatness tolerance (i): precision for curve approximation (0-100)
    pub flatness: f32,

    // Transparency parameters (from ExtGState)
    /// Fill alpha/opacity (CA): 0.0 (transparent) to 1.0 (opaque)
    pub fill_alpha: f32,
    /// Stroke alpha/opacity (ca): 0.0 (transparent) to 1.0 (opaque)
    pub stroke_alpha: f32,
    /// Blend mode (BM): Normal, Multiply, Screen, Overlay, etc.
    pub blend_mode: String,

    // Overprint parameters (from ExtGState, ISO 32000-1 §11.7.4)
    /// Overprint for non-stroking ops (ExtGState `/op`). PDF default `false`.
    pub fill_overprint: bool,
    /// Overprint for stroking ops (ExtGState `/OP`). PDF default `false`.
    pub stroke_overprint: bool,
    /// Overprint mode (ExtGState `/OPM`): 0 = standard, 1 = nonzero
    /// ("Adobe nonzero overprint"). PDF default `0`.
    pub overprint_mode: u8,

    /// Active Form-XObject soft mask (§11.4.7). When `Some`, each paint
    /// operator within the graphics-state scope has its destination
    /// alpha modulated by the rasterised Form XObject's projected
    /// values (alpha channel for /S /Alpha, BT.601 luminance for
    /// /S /Luminosity). PDF default `None` (no soft mask).
    pub smask: Option<SoftMaskForm>,

    /// Active spot ink names paired with their tint values for the most
    /// recent fill paint. Populated by the SetFillColor / SetFillColorN
    /// dispatchers when the active fill colour space is `/Separation`
    /// (ISO 32000-1 §8.6.6.4) or `/DeviceN` (§8.6.6.5). Each tuple is
    /// `(ink_name, subtractive_tint)` matching the source colorant
    /// declaration order; `/All` and `/None` are surfaced verbatim so
    /// the §8.6.6.3 reserved-name branch in the renderer can dispatch
    /// on them. Empty Vec means "no explicit spot ink active" — i.e.
    /// the fill came from Device-family / CIE-based / Indexed source.
    ///
    /// The sidecar's per-paint spot-lane mirror reads this field at
    /// paint time to decide which lanes to write under the §11.7.3
    /// "every object paints every component" + §11.7.4.2 BM split
    /// rules. Process colorants named by a DeviceN /Process attrs dict
    /// (§8.6.6.5) are NOT surfaced here — they ride the CMYK plane
    /// alongside the spot lanes.
    pub fill_spot_inks: Vec<(String, f32)>,
    /// Same as [`Self::fill_spot_inks`], for the stroke side
    /// (`SetStrokeColorN`, §8.6.5.1).
    pub stroke_spot_inks: Vec<(String, f32)>,

    /// Name of the pattern selected by the most recent `scn` operator when
    /// the active fill colour space is `/Pattern` (ISO 32000-1 §8.7.3).
    /// The `scn` operator carries the pattern name (e.g. `/P0`) but the
    /// renderer's colour dispatch only kept the numeric components; this
    /// field retains the name so the `Fill` path can look the pattern
    /// stream up in `Resources/Pattern/<name>` and rasterise it (tiling
    /// pattern cells for `/PatternType 1`). `None` means no pattern is
    /// selected for filling. Cleared whenever a device/CIE fill colour is
    /// set.
    pub fill_pattern_name: Option<String>,
}

/// Subtype of a soft-mask (§11.4.7 / Table 144 `S` field). Alpha uses
/// the rasterised Form XObject's alpha channel as the modulation
/// source; Luminosity uses the BT.601 luma of its RGB.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SoftMaskSubtype {
    /// `/S /Alpha` — modulation source is the form's alpha channel.
    Alpha,
    /// `/S /Luminosity` — modulation source is the form's BT.601 luma.
    Luminosity,
}

/// Form-XObject soft-mask payload (§11.4.7).
#[derive(Clone, Debug)]
pub struct SoftMaskForm {
    /// Reference to the Form XObject that supplies the mask geometry.
    pub form_ref: crate::object::ObjectRef,
    /// Alpha vs Luminosity projection.
    pub subtype: SoftMaskSubtype,
    /// Optional `/BC` backdrop colour. Length matches the Group
    /// colour-space component count (1 for /DeviceGray, 3 for
    /// /DeviceRGB, 4 for /DeviceCMYK). Per §11.4.7 honoured only for
    /// /S /Luminosity.
    pub backdrop: Option<Vec<f32>>,
    /// Optional `/TR` transfer function (PDF Function object).
    pub transfer: Option<crate::object::Object>,
}

impl GraphicsState {
    /// Create a new graphics state with default values.
    ///
    /// # Examples
    ///
    /// ```
    /// use pdf_oxide::content::GraphicsState;
    ///
    /// let state = GraphicsState::new();
    /// assert_eq!(state.font_size, 12.0);
    /// assert_eq!(state.horizontal_scaling, 100.0);
    /// ```
    pub fn new() -> Self {
        Self {
            ctm: Matrix::identity(),
            text_matrix: Matrix::identity(),
            text_line_matrix: Matrix::identity(),
            char_space: 0.0,
            word_space: 0.0,
            horizontal_scaling: 100.0,
            leading: 0.0,
            font_name: None,
            font_size: 12.0,
            text_rise: 0.0,
            render_mode: 0,
            text_wmode: 0,
            fill_color_space: "DeviceGray".to_string(), // PDF default
            stroke_color_space: "DeviceGray".to_string(), // PDF default
            fill_color_rgb: (0.0, 0.0, 0.0),            // Black
            stroke_color_rgb: (0.0, 0.0, 0.0),          // Black
            fill_color_cmyk: None,                      // No CMYK color set initially
            stroke_color_cmyk: None,                    // No CMYK color set initially
            fill_color_components: smallvec::SmallVec::new(), // No explicit fill colour yet
            stroke_color_components: smallvec::SmallVec::new(), // No explicit stroke colour yet
            line_width: 1.0,
            dash_pattern: (Vec::new(), 0.0), // Solid line
            line_cap: 0,                     // Butt cap (PDF default)
            line_join: 0,                    // Miter join (PDF default)
            miter_limit: 10.0,               // PDF default miter limit
            rendering_intent: "RelativeColorimetric".to_string(), // PDF default
            flatness: 1.0,                   // PDF default flatness tolerance
            fill_alpha: 1.0,                 // Fully opaque (PDF default)
            stroke_alpha: 1.0,               // Fully opaque (PDF default)
            blend_mode: "Normal".to_string(), // Normal blend mode (PDF default)
            fill_overprint: false,           // §11.7.4 default
            stroke_overprint: false,         // §11.7.4 default
            overprint_mode: 0,               // §11.7.4 default (standard mode)
            smask: None,                     // §11.4.7 default (no soft mask)
            fill_spot_inks: Vec::new(),      // no spot source yet
            stroke_spot_inks: Vec::new(),    // no spot source yet
            fill_pattern_name: None,         // no fill pattern selected yet
        }
    }

    /// Apply a text-space displacement to the text matrix on the active
    /// writing axis. Returns the resulting `(Δe, Δf)` user-space deltas.
    ///
    /// Per ISO 32000-1:2008 §9.4.4 the show-text operator updates
    /// `Tm := [1 0 0 1 tx 0] × Tm` after horizontal advancement, which by
    /// matrix-multiplication identity adds `tx*a` to `Tm.e` and `tx*b` to
    /// `Tm.f`. In vertical writing mode (WMode 1) the same paragraph of the
    /// spec routes the displacement into the y-column of the pre-multiplier:
    /// `Tm := [1 0 0 1 0 ty] × Tm`, which adds `ty*c` to `Tm.e` and
    /// `ty*d` to `Tm.f`.
    ///
    /// This is the single axis-swap site for the extractor's advance
    /// helpers and the rasterizer's measure path. Horizontal callers pay
    /// only one branch (predicted not-taken) per Tj/TJ operator.
    #[inline]
    pub fn advance_text_matrix(&mut self, displacement: f32) -> (f32, f32) {
        let tm = self.text_matrix;
        let (de, df) = if self.text_wmode == 0 {
            (displacement * tm.a, displacement * tm.b)
        } else {
            (displacement * tm.c, displacement * tm.d)
        };
        self.text_matrix.e += de;
        self.text_matrix.f += df;
        (de, df)
    }

    /// Check if the current line style is dashed (not solid).
    ///
    /// Returns true if the dash pattern is non-empty.
    pub fn is_dashed(&self) -> bool {
        !self.dash_pattern.0.is_empty()
    }

    /// Check if the current line style is a dotted line pattern.
    ///
    /// A dotted line typically has short equal on/off segments.
    pub fn is_dotted(&self) -> bool {
        if self.dash_pattern.0.len() >= 2 {
            let on = self.dash_pattern.0[0];
            let off = self.dash_pattern.0[1];
            // Consider it dotted if on/off are similar and small (< 5 units)
            on < 5.0 && off < 5.0 && (on - off).abs() < 2.0
        } else {
            false
        }
    }
}

impl Default for GraphicsState {
    fn default() -> Self {
        Self::new()
    }
}

/// Stack of graphics states for save/restore operations.
///
/// PDF's q (save) and Q (restore) operators push and pop graphics states.
/// This allows temporary modifications to the graphics state that can be
/// easily reverted.
#[derive(Debug, Clone)]
pub struct GraphicsStateStack {
    stack: Vec<GraphicsState>,
}

impl GraphicsStateStack {
    /// Create a new graphics state stack with an initial state.
    ///
    /// # Examples
    ///
    /// ```
    /// use pdf_oxide::content::GraphicsStateStack;
    ///
    /// let stack = GraphicsStateStack::new();
    /// assert_eq!(stack.depth(), 1);
    /// ```
    pub fn new() -> Self {
        Self {
            stack: vec![GraphicsState::new()],
        }
    }

    /// Get a reference to the current graphics state.
    ///
    /// # Examples
    ///
    /// ```
    /// use pdf_oxide::content::GraphicsStateStack;
    ///
    /// let stack = GraphicsStateStack::new();
    /// let state = stack.current();
    /// assert_eq!(state.font_size, 12.0);
    /// ```
    pub fn current(&self) -> &GraphicsState {
        self.stack.last().expect("Stack should never be empty")
    }

    /// Get a mutable reference to the current graphics state.
    ///
    /// # Examples
    ///
    /// ```
    /// use pdf_oxide::content::GraphicsStateStack;
    ///
    /// let mut stack = GraphicsStateStack::new();
    /// stack.current_mut().font_size = 14.0;
    /// assert_eq!(stack.current().font_size, 14.0);
    /// ```
    pub fn current_mut(&mut self) -> &mut GraphicsState {
        self.stack.last_mut().expect("Stack should never be empty")
    }

    /// Save the current graphics state (q operator).
    ///
    /// Pushes a copy of the current state onto the stack.
    ///
    /// # Examples
    ///
    /// ```
    /// use pdf_oxide::content::GraphicsStateStack;
    ///
    /// let mut stack = GraphicsStateStack::new();
    /// stack.save();
    /// assert_eq!(stack.depth(), 2);
    /// ```
    pub fn save(&mut self) {
        let state = self.current().clone();
        self.stack.push(state);
    }

    /// Restore the previous graphics state (Q operator).
    ///
    /// Pops the current state from the stack. If only one state remains
    /// (the initial state), this operation has no effect.
    ///
    /// # Examples
    ///
    /// ```
    /// use pdf_oxide::content::GraphicsStateStack;
    ///
    /// let mut stack = GraphicsStateStack::new();
    /// stack.save();
    /// stack.save();
    /// stack.restore();
    /// assert_eq!(stack.depth(), 2);
    /// stack.restore();
    /// assert_eq!(stack.depth(), 1);
    /// stack.restore(); // No effect, can't pop last state
    /// assert_eq!(stack.depth(), 1);
    /// ```
    pub fn restore(&mut self) {
        if self.stack.len() > 1 {
            self.stack.pop();
        }
    }

    /// Get the current stack depth.
    ///
    /// The depth is always at least 1 (the initial state).
    pub fn depth(&self) -> usize {
        self.stack.len()
    }
}

impl Default for GraphicsStateStack {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests;
