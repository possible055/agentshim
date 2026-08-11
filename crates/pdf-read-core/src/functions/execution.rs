use super::*;

/// Clamp without panicking on malformed bounds. PDF spec allows arrays we do
/// not trust; `f64::clamp` panics on NaN bounds or `min > max`.
pub(super) fn safe_clamp(v: f64, lo: f64, hi: f64) -> f64 {
    if lo.is_nan() || hi.is_nan() {
        return v;
    }
    let (lo, hi) = if lo <= hi { (lo, hi) } else { (hi, lo) };
    v.clamp(lo, hi)
}

/// Push helper enforcing [`MAX_STACK`]. Used by every code path that grows the
/// operand stack so the cap is uniform across literals, dup, copy, and the
/// numeric/bool result of every operator.
fn push_checked(stack: &mut Vec<Value>, v: Value) -> Result<()> {
    if stack.len() >= MAX_STACK {
        return Err(Error::Type4Runtime(format!(
            "Type 4 stack overflow (max {MAX_STACK})"
        )));
    }
    stack.push(v);
    Ok(())
}

// Stack-growth convention used throughout `execute`:
//
// - [`push_checked`] is used by operators that genuinely *grow* the stack
//   relative to its entry size — number/int/bool literals, `dup`, and `copy`.
//   These need the [`MAX_STACK`] guard because they can drive the stack past
//   the cap from a fully-saturated starting state.
// - Net-shrink and net-neutral operators (arithmetic, comparison, boolean,
//   `cvi`/`cvr`, `not`, `exch`, `index`, `atan`, etc.) call raw `stack.push`
//   after their pops. Each such site has already popped at least as many
//   values as it will push back, so there is provably room and the
//   `push_checked` overhead is unnecessary.
pub(super) fn execute(
    instructions: &[Instruction],
    stack: &mut Vec<Value>,
    budget: &mut usize,
) -> Result<()> {
    for instr in instructions {
        if *budget == 0 {
            return Err(Error::Type4Runtime(format!(
                "Type 4 instruction budget exceeded (max {MAX_INSTRUCTIONS})"
            )));
        }
        *budget -= 1;
        match instr {
            Instruction::NumberLiteral(v) => push_checked(stack, Value::Real(*v))?,
            Instruction::IntLiteral(i) => push_checked(stack, Value::Int(*i))?,
            Instruction::BoolLiteral(b) => push_checked(stack, Value::Bool(*b))?,
            Instruction::Add => numeric_binary(stack, |a, b| Ok(a + b))?,
            Instruction::Sub => numeric_binary(stack, |a, b| Ok(a - b))?,
            Instruction::Mul => numeric_binary(stack, |a, b| Ok(a * b))?,
            // PLRM §8.2 raises `undefinedresult` on `div` by zero, but Acrobat
            // and Poppler instead let IEEE 754 produce the result (±inf for
            // n/0, NaN for 0/0). Match that behaviour so a tint transform
            // that overruns its domain doesn't blow up an otherwise valid
            // page. `idiv` / `mod` stay as runtime errors — integer math has
            // no inf/NaN to fall back to.
            Instruction::Div => numeric_binary(stack, |a, b| Ok(a / b))?,
            Instruction::Idiv => {
                // PLRM §8.2: idiv requires integer operands, returns integer.
                // i64::MIN / -1 overflows; use checked_div to fail safely.
                let b = pop(stack)?.as_int()?;
                let a = pop(stack)?.as_int()?;
                if b == 0 {
                    return Err(Error::Type4Runtime("Type 4 idiv by zero".into()));
                }
                let q = a
                    .checked_div(b)
                    .ok_or_else(|| Error::Type4Runtime("Type 4 idiv integer overflow".into()))?;
                stack.push(Value::Int(q));
            }
            Instruction::Mod => {
                let b = pop(stack)?.as_int()?;
                let a = pop(stack)?.as_int()?;
                if b == 0 {
                    return Err(Error::Type4Runtime("Type 4 mod by zero".into()));
                }
                let r = a
                    .checked_rem(b)
                    .ok_or_else(|| Error::Type4Runtime("Type 4 mod integer overflow".into()))?;
                stack.push(Value::Int(r));
            }
            // PLRM §8.2: `neg` on `i64::MIN` overflows. Use `checked_neg` so
            // the program fails cleanly instead of wrapping silently — matches
            // the explicit overflow path already in use for `idiv`/`mod`.
            Instruction::Neg => {
                let v = pop(stack)?;
                match v {
                    Value::Int(i) => {
                        let n = i.checked_neg().ok_or_else(|| {
                            Error::Type4Runtime("Type 4 integer overflow in neg".into())
                        })?;
                        stack.push(Value::Int(n));
                    }
                    Value::Real(r) => stack.push(Value::Real(-r)),
                    Value::Bool(_) => return Err(typecheck("neg expects a number")),
                }
            }
            // PLRM §8.2: `abs` on `i64::MIN` overflows. Same treatment as `neg`.
            Instruction::Abs => {
                let v = pop(stack)?;
                match v {
                    Value::Int(i) => {
                        let n = i.checked_abs().ok_or_else(|| {
                            Error::Type4Runtime("Type 4 integer overflow in abs".into())
                        })?;
                        stack.push(Value::Int(n));
                    }
                    Value::Real(r) => stack.push(Value::Real(r.abs())),
                    Value::Bool(_) => return Err(typecheck("abs expects a number")),
                }
            }
            Instruction::Ceiling => real_unary_preserve(stack, |a| Ok(a.ceil()))?,
            Instruction::Floor => real_unary_preserve(stack, |a| Ok(a.floor()))?,
            // PLRM §8.2: round goes to the greater of the two surrounding
            // integers (i.e. round-half-toward-+inf). Rust's `f64::round`
            // ties away from zero, so -6.5 would become -7.0 instead of -6.0.
            Instruction::Round => real_unary_preserve(stack, |a| Ok((a + 0.5).floor()))?,
            Instruction::Truncate => real_unary_preserve(stack, |a| Ok(a.trunc()))?,
            // PLRM §8.2: sqrt requires num >= 0; ln/log require num > 0.
            // Invalid inputs raise rangecheck/undefinedresult; we propagate as
            // InvalidPdf rather than letting NaN/-inf reach the renderer.
            Instruction::Sqrt => real_unary(stack, |a| {
                if a < 0.0 || a.is_nan() {
                    Err(Error::Type4Runtime("Type 4 sqrt of negative".into()))
                } else {
                    Ok(a.sqrt())
                }
            })?,
            Instruction::Exp => numeric_binary(stack, |base, exp| Ok(base.powf(exp)))?,
            Instruction::Ln => real_unary(stack, |a| {
                if a <= 0.0 || a.is_nan() {
                    Err(Error::Type4Runtime("Type 4 ln of non-positive".into()))
                } else {
                    Ok(a.ln())
                }
            })?,
            Instruction::Log => real_unary(stack, |a| {
                if a <= 0.0 || a.is_nan() {
                    Err(Error::Type4Runtime("Type 4 log of non-positive".into()))
                } else {
                    Ok(a.log10())
                }
            })?,
            Instruction::Sin => real_unary(stack, |a| Ok(a.to_radians().sin()))?,
            Instruction::Cos => real_unary(stack, |a| Ok(a.to_radians().cos()))?,
            // PLRM §8.2: atan returns the angle in degrees in [0, 360). Rust's
            // atan2().to_degrees() returns (-180, 180]; map negative results
            // back into the spec range.
            Instruction::Atan => {
                let den = pop(stack)?.as_real()?;
                let num = pop(stack)?.as_real()?;
                if num == 0.0 && den == 0.0 {
                    return Err(Error::Type4Runtime(
                        "Type 4 atan undefined for (0, 0)".into(),
                    ));
                }
                let mut deg = num.atan2(den).to_degrees();
                if deg < 0.0 {
                    deg += 360.0;
                }
                // Guard against atan2 returning exactly 360.0 due to rounding.
                if deg >= 360.0 {
                    deg -= 360.0;
                }
                stack.push(Value::Real(deg));
            }
            Instruction::Eq => {
                let b = pop(stack)?;
                let a = pop(stack)?;
                stack.push(Value::Bool(values_equal(a, b)));
            }
            Instruction::Ne => {
                let b = pop(stack)?;
                let a = pop(stack)?;
                stack.push(Value::Bool(!values_equal(a, b)));
            }
            Instruction::Gt => comparison(stack, |o| o == std::cmp::Ordering::Greater)?,
            Instruction::Ge => comparison(stack, |o| o != std::cmp::Ordering::Less)?,
            Instruction::Lt => comparison(stack, |o| o == std::cmp::Ordering::Less)?,
            Instruction::Le => comparison(stack, |o| o != std::cmp::Ordering::Greater)?,
            Instruction::And => bool_or_bitwise(stack, |a, b| a && b, |a, b| a & b)?,
            Instruction::Or => bool_or_bitwise(stack, |a, b| a || b, |a, b| a | b)?,
            Instruction::Xor => bool_or_bitwise(stack, |a, b| a != b, |a, b| a ^ b)?,
            Instruction::Not => {
                let v = pop(stack)?;
                match v {
                    Value::Bool(b) => stack.push(Value::Bool(!b)),
                    Value::Int(i) => stack.push(Value::Int(!i)),
                    Value::Real(_) => {
                        return Err(typecheck("not expects boolean or integer"));
                    }
                }
            }
            // PLRM §8.2: bitshift takes two integers. Magnitudes >= 64 would
            // panic with Rust's `<<`/`>>`; PLRM specifies "bits shifted out
            // are discarded; zeros are supplied for vacated bits", which for
            // |shift| >= 64 produces 0. We saturate to zero rather than
            // raising a runtime error because the spec gives a defined value.
            Instruction::Bitshift => {
                let shift = pop(stack)?.as_int()?;
                let val = pop(stack)?.as_int()?;
                let result = if shift >= 64 || shift <= -64 {
                    0
                } else if shift >= 0 {
                    val.wrapping_shl(shift as u32)
                } else {
                    // Logical right shift on the unsigned bit pattern, per
                    // PLRM's "bits shifted out are discarded; zeros are
                    // supplied for vacated bits".
                    ((val as u64) >> (-shift) as u32) as i64
                };
                stack.push(Value::Int(result));
            }
            // PLRM §8.2: `cvi` pops a number, truncates toward zero, and
            // pushes the result as a typed integer. Reals outside the i64
            // range overflow as a runtime error rather than wrapping.
            Instruction::Cvi => {
                let v = pop(stack)?;
                match v {
                    Value::Int(i) => stack.push(Value::Int(i)),
                    Value::Real(r) => {
                        if !r.is_finite() {
                            return Err(Error::Type4Runtime(
                                "Type 4 cvi: input is not finite".into(),
                            ));
                        }
                        let t = r.trunc();
                        // i64::MAX (2^63 - 1) is NOT exactly representable in
                        // f64 — `i64::MAX as f64` rounds up to exactly 2^63,
                        // which IS representable. So the upper bound has to
                        // use 2^63 with a `>=` comparison; using `> i64::MAX
                        // as f64` lets t == 2^63 slip through and saturate
                        // silently to i64::MAX on the `as i64` cast below.
                        const I64_MAX_PLUS_ONE_AS_F64: f64 = 9_223_372_036_854_775_808.0;
                        if t < i64::MIN as f64 || t >= I64_MAX_PLUS_ONE_AS_F64 {
                            return Err(Error::Type4Runtime("Type 4 cvi: integer overflow".into()));
                        }
                        stack.push(Value::Int(t as i64));
                    }
                    Value::Bool(_) => return Err(typecheck("cvi expects a number")),
                }
            }
            // PLRM §8.2: `cvr` pops a number and pushes it as a typed real.
            // An integer becomes a typed real (no longer satisfies `as_int`).
            Instruction::Cvr => {
                let v = pop(stack)?;
                match v {
                    Value::Int(i) => stack.push(Value::Real(i as f64)),
                    Value::Real(r) => stack.push(Value::Real(r)),
                    Value::Bool(_) => return Err(typecheck("cvr expects a number")),
                }
            }
            Instruction::Dup => {
                let a = *stack.last().ok_or_else(underflow)?;
                push_checked(stack, a)?;
            }
            Instruction::Exch => {
                let b = pop(stack)?;
                let a = pop(stack)?;
                // Net-neutral: two pops, two pushes back.
                stack.push(b);
                stack.push(a);
            }
            Instruction::Pop => {
                pop(stack)?;
            }
            Instruction::Copy => {
                // Note: operand pops happen before the bounds check below.
                // The mutation is visible only inside this scope; on error
                // the evaluation aborts and the partially-popped stack is
                // never observed by the caller.
                let n = pop_count(stack, "copy")?;
                if n > stack.len() {
                    return Err(underflow());
                }
                if stack.len().checked_add(n).is_none_or(|new| new > MAX_STACK) {
                    return Err(Error::Type4Runtime(format!(
                        "Type 4 stack overflow during copy (max {MAX_STACK})"
                    )));
                }
                let start = stack.len() - n;
                let copied: Vec<Value> = stack[start..].to_vec();
                stack.extend_from_slice(&copied);
            }
            Instruction::Index => {
                // Note: operand pops happen before the bounds check below.
                // The mutation is visible only inside this scope; on error
                // the evaluation aborts and the partially-popped stack is
                // never observed by the caller.
                let n = pop_count(stack, "index")?;
                if n >= stack.len() {
                    return Err(underflow());
                }
                let val = stack[stack.len() - 1 - n];
                stack.push(val);
            }
            Instruction::Roll => {
                // Note: operand pops happen before the bounds check below.
                // The mutation is visible only inside this scope; on error
                // the evaluation aborts and the partially-popped stack is
                // never observed by the caller.
                let j = pop(stack)?.as_int()?;
                let n = pop_count(stack, "roll")?;
                if n > stack.len() {
                    return Err(underflow());
                }
                if n > 0 {
                    let start = stack.len() - n;
                    let slice = &mut stack[start..];
                    let len = slice.len() as i64;
                    let shift = j.rem_euclid(len) as usize;
                    slice.rotate_right(shift);
                }
            }
            Instruction::If(body) => {
                let cond = pop(stack)?.as_bool()?;
                if cond {
                    execute(body, stack, budget)?;
                }
            }
            Instruction::IfElse(true_branch, false_branch) => {
                let cond = pop(stack)?.as_bool()?;
                if cond {
                    execute(true_branch, stack, budget)?;
                } else {
                    execute(false_branch, stack, budget)?;
                }
            }
            Instruction::ProcedureBody(_) => {
                // Unreachable: `resolve_conditionals` rejects orphan procedure
                // bodies at parse time. If one reaches `execute` we treat it
                // as an internal invariant violation rather than panicking.
                return Err(Error::Type4Runtime(
                    "Type 4 internal error: ProcedureBody reached execute".into(),
                ));
            }
        }
    }
    Ok(())
}

fn pop(stack: &mut Vec<Value>) -> Result<Value> {
    stack.pop().ok_or_else(underflow)
}

/// Pop a non-negative count for `copy`/`index`/`roll`. PLRM rejects negative
/// or non-integer counts with `rangecheck`/`typecheck`; `as usize` on negative
/// or NaN floats would silently wrap.
fn pop_count(stack: &mut Vec<Value>, op: &str) -> Result<usize> {
    let v = pop(stack)?.as_int()?;
    if v < 0 {
        return Err(Error::Type4Runtime(format!(
            "Type 4 {op}: negative count {v}"
        )));
    }
    Ok(v as usize)
}

fn underflow() -> Error {
    Error::Type4Runtime("Type 4 stack underflow".into())
}

pub(super) fn typecheck(msg: &str) -> Error {
    Error::Type4Runtime(format!("Type 4 typecheck: {msg}"))
}

fn real_unary(stack: &mut Vec<Value>, f: impl FnOnce(f64) -> Result<f64>) -> Result<()> {
    let a = pop(stack)?.as_real()?;
    stack.push(Value::Real(f(a)?));
    Ok(())
}

/// Unary operator that preserves integer-ness if the input was an integer
/// (e.g. `ceiling`, `floor`, `round`, `truncate` per PLRM §8.2).
fn real_unary_preserve(stack: &mut Vec<Value>, f: impl FnOnce(f64) -> Result<f64>) -> Result<()> {
    let v = pop(stack)?;
    match v {
        Value::Int(i) => stack.push(Value::Int(i)),
        Value::Real(r) => stack.push(Value::Real(f(r)?)),
        Value::Bool(_) => return Err(typecheck("expected number, got boolean")),
    }
    Ok(())
}

/// Arithmetic with PLRM type promotion: integer op integer -> integer (if no
/// overflow on add/sub/mul; we fall back to real on overflow), otherwise real.
fn numeric_binary(stack: &mut Vec<Value>, f: impl FnOnce(f64, f64) -> Result<f64>) -> Result<()> {
    let b = pop(stack)?;
    let a = pop(stack)?;
    let af = a.as_real()?;
    let bf = b.as_real()?;
    let result = f(af, bf)?;
    // Promote back to Int when both operands were integers and the result is
    // exactly representable. This keeps `52 not` working when authors wrap
    // bitwise ops around arithmetic chains.
    if matches!(a, Value::Int(_))
        && matches!(b, Value::Int(_))
        && result.is_finite()
        && result.fract() == 0.0
        && result >= i64::MIN as f64
        && result <= i64::MAX as f64
    {
        stack.push(Value::Int(result as i64));
    } else {
        stack.push(Value::Real(result));
    }
    Ok(())
}

fn comparison(stack: &mut Vec<Value>, pred: impl FnOnce(std::cmp::Ordering) -> bool) -> Result<()> {
    let b = pop(stack)?.as_real()?;
    let a = pop(stack)?.as_real()?;
    let ord = a
        .partial_cmp(&b)
        .ok_or_else(|| Error::Type4Runtime("Type 4 comparison with NaN".into()))?;
    stack.push(Value::Bool(pred(ord)));
    Ok(())
}

fn values_equal(a: Value, b: Value) -> bool {
    match (a, b) {
        (Value::Bool(x), Value::Bool(y)) => x == y,
        (Value::Bool(_), _) | (_, Value::Bool(_)) => false,
        // PLRM treats `1` and `1.0` as equal, so compare numerically.
        _ => a.as_real().ok() == b.as_real().ok(),
    }
}

/// `and`/`or`/`xor`: PLRM §8.2 dispatches on operand type — both boolean uses
/// logical op, both integer uses bitwise. Mixed types are a typecheck error.
fn bool_or_bitwise(
    stack: &mut Vec<Value>,
    boolean: impl FnOnce(bool, bool) -> bool,
    bitwise: impl FnOnce(i64, i64) -> i64,
) -> Result<()> {
    let b = pop(stack)?;
    let a = pop(stack)?;
    match (a, b) {
        (Value::Bool(x), Value::Bool(y)) => stack.push(Value::Bool(boolean(x, y))),
        (Value::Int(x), Value::Int(y)) => stack.push(Value::Int(bitwise(x, y))),
        _ => {
            return Err(typecheck(
                "and/or/xor require matching boolean or integer operands",
            ))
        }
    }
    Ok(())
}
