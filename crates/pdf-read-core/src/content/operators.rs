//! PDF content stream operators.
//!
//! This module defines the operator types used in PDF content streams.
//! Content streams contain a sequence of operators that define the appearance
//! of a page, including text positioning, graphics state, and colors.

use crate::object::Object;

/// A content stream operator.
#[derive(Debug, Clone, PartialEq)]
#[allow(clippy::box_collection)] // Intentional: Boxing reduces enum from 112 to 40 bytes
pub enum Operator {
    // Text positioning operators
    /// Move text position (Td)
    Td {
        /// Horizontal offset
        tx: f32,
        /// Vertical offset
        ty: f32,
    },
    /// Move text position and set leading (TD)
    TD {
        /// Horizontal offset
        tx: f32,
        /// Vertical offset
        ty: f32,
    },
    /// Set text matrix (Tm)
    Tm {
        /// Matrix element a
        a: f32,
        /// Matrix element b
        b: f32,
        /// Matrix element c
        c: f32,
        /// Matrix element d
        d: f32,
        /// Matrix element e (x translation)
        e: f32,
        /// Matrix element f (y translation)
        f: f32,
    },
    /// Move to start of next line (T*)
    TStar,

    // Text showing operators
    /// Show text string (Tj)
    Tj {
        /// Text to show (byte array)
        text: Vec<u8>,
    },
    /// Show text with individual glyph positioning (TJ)
    TJ {
        /// Array of text strings and positioning adjustments
        array: Vec<TextElement>,
    },
    /// Move to next line and show text (')
    Quote {
        /// Text to show
        text: Vec<u8>,
    },
    /// Set spacing and show text (")
    DoubleQuote {
        /// Word spacing
        word_space: f32,
        /// Character spacing
        char_space: f32,
        /// Text to show
        text: Vec<u8>,
    },

    // Text state operators
    /// Set character spacing (Tc)
    Tc {
        /// Character spacing
        char_space: f32,
    },
    /// Set word spacing (Tw)
    Tw {
        /// Word spacing
        word_space: f32,
    },
    /// Set horizontal scaling (Tz)
    Tz {
        /// Horizontal scaling percentage
        scale: f32,
    },
    /// Set text leading (TL)
    TL {
        /// Text leading
        leading: f32,
    },
    /// Set font and size (Tf)
    Tf {
        /// Font name
        font: String,
        /// Font size
        size: f32,
    },
    /// Set text rendering mode (Tr)
    Tr {
        /// Rendering mode
        render: u8,
    },
    /// Set text rise (Ts)
    Ts {
        /// Text rise
        rise: f32,
    },

    // Graphics state operators
    /// Save graphics state (q)
    SaveState,
    /// Restore graphics state (Q)
    RestoreState,
    /// Modify current transformation matrix (cm)
    Cm {
        /// Matrix element a
        a: f32,
        /// Matrix element b
        b: f32,
        /// Matrix element c
        c: f32,
        /// Matrix element d
        d: f32,
        /// Matrix element e (x translation)
        e: f32,
        /// Matrix element f (y translation)
        f: f32,
    },

    // Color operators
    /// Set RGB fill color (rg)
    SetFillRgb {
        /// Red component (0.0-1.0)
        r: f32,
        /// Green component (0.0-1.0)
        g: f32,
        /// Blue component (0.0-1.0)
        b: f32,
    },
    /// Set RGB stroke color (RG)
    SetStrokeRgb {
        /// Red component (0.0-1.0)
        r: f32,
        /// Green component (0.0-1.0)
        g: f32,
        /// Blue component (0.0-1.0)
        b: f32,
    },
    /// Set gray fill color (g)
    SetFillGray {
        /// Gray level (0.0-1.0)
        gray: f32,
    },
    /// Set gray stroke color (G)
    SetStrokeGray {
        /// Gray level (0.0-1.0)
        gray: f32,
    },
    /// Set CMYK fill color (k)
    SetFillCmyk {
        /// Cyan component (0.0-1.0)
        c: f32,
        /// Magenta component (0.0-1.0)
        m: f32,
        /// Yellow component (0.0-1.0)
        y: f32,
        /// Black component (0.0-1.0)
        k: f32,
    },
    /// Set CMYK stroke color (K)
    SetStrokeCmyk {
        /// Cyan component (0.0-1.0)
        c: f32,
        /// Magenta component (0.0-1.0)
        m: f32,
        /// Yellow component (0.0-1.0)
        y: f32,
        /// Black component (0.0-1.0)
        k: f32,
    },

    // Color space operators
    /// Set fill color space (cs)
    ///
    /// PDF Spec: ISO 32000-1:2008, Section 8.6.4 - Color Space Operators
    SetFillColorSpace {
        /// Color space name (e.g., "DeviceRGB", "DeviceCMYK", "DeviceGray")
        name: String,
    },
    /// Set stroke color space (CS)
    ///
    /// PDF Spec: ISO 32000-1:2008, Section 8.6.4 - Color Space Operators
    SetStrokeColorSpace {
        /// Color space name (e.g., "DeviceRGB", "DeviceCMYK", "DeviceGray")
        name: String,
    },
    /// Set fill color (sc)
    ///
    /// Sets color components in the current fill color space.
    /// Number of components depends on color space (1 for Gray, 3 for RGB, 4 for CMYK).
    SetFillColor {
        /// Color components (length depends on color space)
        components: Vec<f32>,
    },
    /// Set stroke color (SC)
    ///
    /// Sets color components in the current stroke color space.
    /// Number of components depends on color space (1 for Gray, 3 for RGB, 4 for CMYK).
    SetStrokeColor {
        /// Color components (length depends on color space)
        components: Vec<f32>,
    },
    /// Set fill color with named pattern (scn)
    ///
    /// Like sc, but also supports pattern color spaces with an optional pattern name.
    SetFillColorN {
        /// Color components (may be empty for patterns)
        components: Vec<f32>,
        /// Optional pattern name for pattern color spaces.
        /// Boxed to reduce Operator enum size (`Option<String>` is 24 bytes → 8 bytes).
        name: Option<Box<String>>,
    },
    /// Set stroke color with named pattern (SCN)
    ///
    /// Like SC, but also supports pattern color spaces with an optional pattern name.
    SetStrokeColorN {
        /// Color components (may be empty for patterns)
        components: Vec<f32>,
        /// Optional pattern name for pattern color spaces.
        /// Boxed to reduce Operator enum size.
        name: Option<Box<String>>,
    },

    // Text object operators
    /// Begin text object (BT)
    BeginText,
    /// End text object (ET)
    EndText,

    // XObject operators
    /// Paint XObject (Do)
    Do {
        /// XObject name
        name: String,
    },

    // Path construction and painting (minimal support for now)
    /// Move to (m)
    MoveTo {
        /// X coordinate
        x: f32,
        /// Y coordinate
        y: f32,
    },
    /// Line to (l)
    LineTo {
        /// X coordinate
        x: f32,
        /// Y coordinate
        y: f32,
    },
    /// Cubic Bézier curve (c)
    CurveTo {
        /// X coordinate of first control point
        x1: f32,
        /// Y coordinate of first control point
        y1: f32,
        /// X coordinate of second control point
        x2: f32,
        /// Y coordinate of second control point
        y2: f32,
        /// X coordinate of end point
        x3: f32,
        /// Y coordinate of end point
        y3: f32,
    },
    /// Bézier curve with first control point = current point (v)
    CurveToV {
        /// X coordinate of second control point
        x2: f32,
        /// Y coordinate of second control point
        y2: f32,
        /// X coordinate of end point
        x3: f32,
        /// Y coordinate of end point
        y3: f32,
    },
    /// Bézier curve with second control point = end point (y)
    CurveToY {
        /// X coordinate of first control point
        x1: f32,
        /// Y coordinate of first control point
        y1: f32,
        /// X coordinate of end point
        x3: f32,
        /// Y coordinate of end point
        y3: f32,
    },
    /// Close current subpath (h)
    ClosePath,
    /// Rectangle (re)
    Rectangle {
        /// X coordinate
        x: f32,
        /// Y coordinate
        y: f32,
        /// Width
        width: f32,
        /// Height
        height: f32,
    },
    /// Stroke path (S)
    Stroke,
    /// Fill path (f)
    Fill,
    /// Fill path (even-odd) (f*)
    FillEvenOdd,
    /// Close, fill and stroke (b)
    CloseFillStroke,
    /// Fill and stroke using nonzero winding rule (B)
    FillStroke,
    /// Fill and stroke using even-odd rule (B*)
    FillStrokeEvenOdd,
    /// Close, fill and stroke using even-odd rule (b*)
    CloseFillStrokeEvenOdd,
    /// End path without filling or stroking (n)
    EndPath,
    /// Modify clipping path using non-zero winding rule (W)
    ClipNonZero,
    /// Modify clipping path using even-odd rule (W*)
    ClipEvenOdd,

    // Graphics state operators
    /// Set line width (w)
    SetLineWidth {
        /// Line width
        width: f32,
    },
    /// Set line dash pattern (d)
    SetDash {
        /// Dash array ([on1, off1, on2, off2, ...])
        array: Vec<f32>,
        /// Dash phase (offset into pattern)
        phase: f32,
    },
    /// Set line cap style (J)
    ///
    /// PDF Spec: ISO 32000-1:2008, Section 8.4.3.3 - Line Cap Style
    SetLineCap {
        /// Line cap style: 0=butt cap, 1=round cap, 2=projecting square cap
        cap_style: u8,
    },
    /// Set line join style (j)
    ///
    /// PDF Spec: ISO 32000-1:2008, Section 8.4.3.4 - Line Join Style
    SetLineJoin {
        /// Line join style: 0=miter join, 1=round join, 2=bevel join
        join_style: u8,
    },
    /// Set miter limit (M)
    ///
    /// PDF Spec: ISO 32000-1:2008, Section 8.4.3.5 - Miter Limit
    SetMiterLimit {
        /// Miter limit (ratio of miter length to line width)
        limit: f32,
    },
    /// Set rendering intent (ri)
    ///
    /// PDF Spec: ISO 32000-1:2008, Section 8.6.5.8 - Rendering Intents
    SetRenderingIntent {
        /// Rendering intent: AbsoluteColorimetric, RelativeColorimetric, Saturation, or Perceptual
        intent: String,
    },
    /// Set flatness tolerance (i)
    ///
    /// PDF Spec: ISO 32000-1:2008, Section 6.5.1 - Flatness Tolerance
    SetFlatness {
        /// Flatness tolerance (0-100, controlling curve approximation quality)
        tolerance: f32,
    },
    /// Set extended graphics state (gs)
    ///
    /// PDF Spec: ISO 32000-1:2008, Section 8.4.5 - Graphics State Parameter Dictionaries
    ///
    /// References an ExtGState dictionary in the page resources that contains
    /// graphics state parameters like transparency, blend modes, and line styles.
    SetExtGState {
        /// Name of the ExtGState dictionary in /ExtGState resources
        dict_name: String,
    },
    /// Paint shading pattern (sh)
    ///
    /// PDF Spec: ISO 32000-1:2008, Section 8.7.4.3 - Shading Patterns
    ///
    /// Paints a shading pattern (gradient) defined in the /Shading resource dictionary.
    /// Shading types include: Function-based, Axial, Radial, Free-form Gouraud, Lattice-form Gouraud, Coons patch, Tensor-product patch.
    PaintShading {
        /// Name of the shading dictionary in /Shading resources
        name: String,
    },

    // Inline image operator
    // PDF Spec: ISO 32000-1:2008, Section 8.9.7 - Inline Images
    /// Inline image (BI...ID...EI sequence)
    ///
    /// Represents a complete inline image sequence from BI (begin inline image)
    /// through ID (inline image data) to EI (end inline image).
    ///
    /// Inline images are small images embedded directly in the content stream
    /// rather than referenced as XObjects. The dictionary contains abbreviated
    /// keys for image properties.
    ///
    /// Common dictionary keys (abbreviated):
    /// - W: Width (required)
    /// - H: Height (required)
    /// - CS: ColorSpace (e.g., /DeviceRGB, /DeviceGray)
    /// - BPC: BitsPerComponent (typically 1, 8)
    /// - F: Filter (e.g., /FlateDecode, /DCTDecode)
    /// - DP: DecodeParms (decode parameters for filter)
    /// - I: Interpolate (boolean)
    InlineImage {
        /// Inline image dictionary with abbreviated keys.
        /// Boxed to reduce Operator enum size (HashMap is 48 bytes).
        dict: Box<std::collections::HashMap<String, Object>>,
        /// Raw image data bytes (possibly compressed)
        data: Vec<u8>,
    },

    // Marked content operators (for tagged PDF structure)
    // PDF Spec: ISO 32000-1:2008, Section 14.6 - Marked Content
    /// Begin marked content (BMC)
    ///
    /// Begins a marked content sequence identified by a tag.
    /// Used for logical structure and accessibility in tagged PDFs.
    BeginMarkedContent {
        /// Tag name identifying the marked content
        tag: String,
    },
    /// Begin marked content with property list (BDC)
    ///
    /// Begins a marked content sequence with associated properties.
    /// The properties can be inline (dictionary) or a reference to a properties resource.
    BeginMarkedContentDict {
        /// Tag name identifying the marked content
        tag: String,
        /// Properties (dictionary or name reference to /Properties resource).
        /// Boxed to reduce Operator enum size from 112 to 56 bytes (Object is 88 bytes).
        properties: Box<Object>,
    },
    /// End marked content (EMC)
    ///
    /// Ends the most recent marked content sequence.
    /// Must be balanced with BMC or BDC operators.
    EndMarkedContent,

    // Unknown operator (for operators we don't handle yet)
    /// Other operator
    Other {
        /// Operator name
        name: String,
        /// Operands. Boxed to reduce Operator enum size.
        operands: Box<Vec<Object>>,
    },
}

/// Element in a TJ array (text showing with positioning).
#[derive(Debug, Clone, PartialEq)]
pub enum TextElement {
    /// Text string to show
    String(Vec<u8>),
    /// Positioning adjustment (in thousandths of a unit of text space)
    Offset(f32),
}

mod methods;

#[cfg(test)]
mod tests;
