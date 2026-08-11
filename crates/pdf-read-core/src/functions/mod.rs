// SPDX-License-Identifier: MIT OR Apache-2.0

//! PostScript Type 4 (calculator) function evaluator.
//!
//! PDF Type 4 functions are small stack-based programs used as tint transforms
//! in Separation and DeviceN color spaces. This module parses and evaluates
//! them per ISO 32000-1:2008 §7.10.5 and Table 42, which together define a
//! restricted subset of the PostScript Language Reference Manual (PLRM, 3rd
//! ed.) §8.2 operator semantics. Where Rust's default numeric behaviour
//! diverges from PLRM (e.g. `f64::round` ties, `atan2` range, panicking on
//! `i64::MIN / -1`), the PLRM rule is honoured and cited inline.
//!
//! # Supported operators
//!
//! Per PDF spec ISO 32000-1 §7.10.5 / Table 42:
//!
//! - **Arithmetic**: `add` `sub` `mul` `div` `idiv` `mod` `neg` `abs`
//!   `ceiling` `floor` `round` `truncate` `sqrt` `exp` `ln` `log`
//!   `sin` `cos` `atan` `cvi` `cvr`
//! - **Comparison**: `eq` `ne` `gt` `ge` `lt` `le`
//! - **Boolean / bitwise**: `and` `or` `xor` `not` `bitshift`
//! - **Stack**: `dup` `exch` `pop` `copy` `index` `roll`
//! - **Conditional**: `if` `ifelse` (each consuming one or two preceding
//!   `{ ... }` procedure bodies)
//! - **Literals**: integer (`5`, `-3`, `+1`), real (`1.5`, `-.5`, `1e-3`),
//!   boolean (`true`, `false`)
//!
//! Non-finite numeric literals (`inf`, `NaN`) are rejected at parse time.
//!
//! # Integration
//!
//! The renderer at `src/rendering/page_renderer.rs` (lines 566-642) currently
//! handles Type 2 (exponential interpolation) tint transforms and falls back
//! to grayscale for everything else. To support Type 4, add a branch for
//! `FunctionType == 4`: decode the function stream, then call
//! `evaluate_type4(stream_bytes, &[tint])` to get CMYK components.

#![forbid(unsafe_code)]

use crate::error::{Error, Result};

mod execution;
mod parser;

use execution::{execute, safe_clamp, typecheck};
use parser::parse;

/// A parsed instruction in a Type 4 PostScript calculator program.
#[derive(Debug, Clone, PartialEq)]
enum Instruction {
    NumberLiteral(f64),
    IntLiteral(i64),
    BoolLiteral(bool),
    // Arithmetic
    Add,
    Sub,
    Mul,
    Div,
    Idiv,
    Mod,
    Neg,
    Abs,
    Ceiling,
    Floor,
    Round,
    Truncate,
    Sqrt,
    Exp,
    Ln,
    Log,
    Sin,
    Cos,
    Atan,
    // Comparison
    Eq,
    Ne,
    Gt,
    Ge,
    Lt,
    Le,
    // Boolean/bitwise
    And,
    Or,
    Xor,
    Not,
    Bitshift,
    // Type conversion (PLRM §8.2)
    Cvi,
    Cvr,
    // Stack manipulation
    Dup,
    Exch,
    Pop,
    Copy,
    Index,
    Roll,
    // Parser-emitted procedure body. A `{ ... }` block starts as one of these
    // and is consumed by the immediately following `if` or `ifelse` during
    // `resolve_conditionals`. Any `ProcedureBody` that survives the resolve
    // pass is an orphan and rejected at parse time.
    ProcedureBody(Vec<Instruction>),
    // Conditional (post-resolve form)
    If(Vec<Instruction>),
    IfElse(Vec<Instruction>, Vec<Instruction>),
}

/// A runtime stack value. PLRM §8.2 distinguishes integer, real, and boolean
/// types; the same surface syntax (`1`, `1.0`, `true`) can produce values that
/// behave differently under `not`, `and`, `or`, `xor`, `idiv`, and `mod`.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Value {
    Int(i64),
    Real(f64),
    Bool(bool),
}

impl Value {
    fn as_real(self) -> Result<f64> {
        match self {
            Value::Int(i) => Ok(i as f64),
            Value::Real(r) => Ok(r),
            Value::Bool(_) => Err(typecheck("expected numeric, got boolean")),
        }
    }

    fn as_int(self) -> Result<i64> {
        match self {
            Value::Int(i) => Ok(i),
            // PLRM §8.2: idiv, mod, bitshift, and other integer ops require
            // typed integer values. A real literal like `2.0` is a typed real
            // even if its value is integral and is rejected.
            Value::Real(_) => Err(typecheck("expected integer, got real")),
            Value::Bool(_) => Err(typecheck("expected integer, got boolean")),
        }
    }

    fn as_bool(self) -> Result<bool> {
        match self {
            Value::Bool(b) => Ok(b),
            _ => Err(typecheck("expected boolean")),
        }
    }

    fn to_output(self) -> f64 {
        match self {
            Value::Int(i) => i as f64,
            Value::Real(r) => r,
            Value::Bool(b) => {
                if b {
                    1.0
                } else {
                    0.0
                }
            }
        }
    }
}

/// Maximum nested `{ ... }` depth permitted by the parser. PLRM has no formal
/// cap, but real-world Type 4 streams are shallow; bounding it prevents a
/// maliciously deep stream from blowing the Rust call stack since `parse_body`
/// recurses for each brace level. Programs nested deeper return
/// [`Error::InvalidPdf`].
pub const MAX_PARSE_DEPTH: u32 = 32;

/// Maximum operand stack size during execution. PLRM §7.10.5.2 requires a
/// "stack overflow" diagnostic; we surface it as [`Error::Type4Runtime`]. The
/// cap matches what Adobe accepts in practice — Acrobat's interpreter allows
/// up to a few hundred operands.
pub const MAX_STACK: usize = 256;

/// Maximum number of instructions the evaluator will execute. Type 4 has no
/// loops in the language proper, but nested `if`/`ifelse` plus large generated
/// bodies (or pathological streams crafted to consume CPU) can still produce
/// arbitrarily many steps. 100 000 is generous for any realistic tint
/// transform while still being a hard upper bound. Programs that exceed this
/// budget return [`Error::Type4Runtime`].
pub const MAX_INSTRUCTIONS: usize = 100_000;

/// A compiled Type 4 PostScript calculator program.
///
/// Construct once via [`Program::compile`], then call [`Program::evaluate`]
/// (or [`Program::evaluate_clamped`]) many times with different inputs. This
/// is the recommended path for tint transforms in Separation and DeviceN
/// colour spaces, which are evaluated per pixel — parsing each call would
/// be wasteful.
///
/// `Program` is `Send + Sync`, so it can live inside a shared tint-transform
/// cache without a `Mutex`.
#[derive(Debug, Clone)]
pub struct Program {
    instructions: Vec<Instruction>,
}

impl Program {
    /// Compile a Type 4 program from its raw bytes.
    ///
    /// Returns [`Error::InvalidPdf`] for any parse-time failure — syntax
    /// errors (missing braces, unknown tokens, orphan procedure bodies),
    /// non-finite numeric literals, and resource caps that fire during
    /// parsing such as nesting deeper than [`MAX_PARSE_DEPTH`]. All of these
    /// are deterministic properties of the program text, so callers must
    /// not retry the same bytes on the next sample.
    ///
    /// [`Error::Type4Runtime`] is reserved for execution-time failures
    /// (stack overflow/underflow, integer overflow in arithmetic, hitting
    /// the per-call instruction budget); those are raised by
    /// [`evaluate`](Self::evaluate) and [`evaluate_clamped`](Self::evaluate_clamped),
    /// never by `compile`. The resulting `Program` is reusable across many
    /// `evaluate` calls.
    pub fn compile(bytes: &[u8]) -> Result<Self> {
        Ok(Self {
            instructions: parse(bytes)?,
        })
    }

    /// Evaluate the compiled program against the given inputs.
    ///
    /// Each call starts with a fresh operand stack initialised from `inputs`;
    /// the compiled `Program` itself carries no mutable evaluation state, so
    /// concurrent calls (across threads or pixels) are safe and independent.
    pub fn evaluate(&self, inputs: &[f64]) -> Result<Vec<f64>> {
        if inputs.len() > MAX_STACK {
            return Err(Error::Type4Runtime(format!(
                "Type 4 stack overflow: {} inputs exceeds max {MAX_STACK}",
                inputs.len()
            )));
        }
        // The public API takes f64s. Caller intent (typed int vs typed real)
        // is ambiguous, so promote exact integer-valued inputs to typed
        // integers. This lets integer ops (idiv, mod, bitshift) accept
        // caller-supplied integer inputs while still rejecting parser
        // literals like `2.0`.
        // i64::MAX (2^63 - 1) is not exactly representable in f64 —
        // `i64::MAX as f64` rounds up to exactly 2^63. So using
        // `v <= i64::MAX as f64` lets v == 2^63 through, where the
        // `v as i64` cast then saturates silently to i64::MAX. Use 2^63
        // with a strict `<` to keep the boundary on the safe side.
        const I64_MAX_PLUS_ONE_AS_F64: f64 = 9_223_372_036_854_775_808.0;
        let mut stack: Vec<Value> = inputs
            .iter()
            .map(|&v| {
                if v.is_finite()
                    && v.fract() == 0.0
                    && v >= i64::MIN as f64
                    && v < I64_MAX_PLUS_ONE_AS_F64
                {
                    Value::Int(v as i64)
                } else {
                    Value::Real(v)
                }
            })
            .collect();
        let mut budget = MAX_INSTRUCTIONS;
        execute(&self.instructions, &mut stack, &mut budget)?;
        Ok(stack.into_iter().map(Value::to_output).collect())
    }

    /// Evaluate with Domain/Range clamping per the PDF function dictionary.
    ///
    /// `domain` is a list of `[min, max]` pairs (one per input). Each input is
    /// clamped to its domain before execution. `range` is a list of
    /// `[min, max]` pairs (one per output). Each output is clamped to its
    /// range after execution. Malformed bounds (`min > max`) are swapped;
    /// NaN bounds are treated as no-op, since `f64::clamp` would otherwise
    /// panic.
    pub fn evaluate_clamped(
        &self,
        inputs: &[f64],
        domain: &[[f64; 2]],
        range: &[[f64; 2]],
    ) -> Result<Vec<f64>> {
        let clamped_inputs: Vec<f64> = inputs
            .iter()
            .enumerate()
            .map(|(i, &v)| {
                if let Some(&[lo, hi]) = domain.get(i) {
                    safe_clamp(v, lo, hi)
                } else {
                    v
                }
            })
            .collect();
        let mut result = self.evaluate(&clamped_inputs)?;
        for (i, val) in result.iter_mut().enumerate() {
            if let Some(&[lo, hi]) = range.get(i) {
                *val = safe_clamp(*val, lo, hi);
            }
        }
        Ok(result)
    }
}

/// Evaluate a Type 4 PostScript calculator program.
///
/// `program` is the raw stream content (e.g. `{ dup 0.84 mul ... }`).
/// `inputs` are pushed onto the stack before execution.
/// After execution the remaining stack values are returned as the output.
///
/// This compiles and evaluates the program in one shot. For per-pixel
/// evaluation (e.g. a Separation tint transform applied to every sample),
/// use [`Program::compile`] once and call [`Program::evaluate`] many times.
pub fn evaluate_type4(program: &[u8], inputs: &[f64]) -> Result<Vec<f64>> {
    Program::compile(program)?.evaluate(inputs)
}

/// Evaluate with Domain/Range clamping per the PDF function dictionary.
///
/// Thin wrapper around [`Program::compile`] + [`Program::evaluate_clamped`].
/// Same per-call-cost caveat as [`evaluate_type4`].
pub fn evaluate_type4_clamped(
    program: &[u8],
    inputs: &[f64],
    domain: &[[f64; 2]],
    range: &[[f64; 2]],
) -> Result<Vec<f64>> {
    Program::compile(program)?.evaluate_clamped(inputs, domain, range)
}

#[cfg(test)]
mod tests;
