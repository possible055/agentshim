use super::*;

/// Parse a Type 4 PostScript calculator program from raw bytes.
///
/// The program must be enclosed in `{ }`. Nested braces define procedure
/// bodies used with `if` and `ifelse`.
pub(super) fn parse(program: &[u8]) -> Result<Vec<Instruction>> {
    let s = std::str::from_utf8(program)
        .map_err(|e| Error::InvalidPdf(format!("Type 4 function is not valid UTF-8: {e}")))?;
    let s = s.trim();
    if !s.starts_with('{') || !s.ends_with('}') {
        return Err(Error::InvalidPdf(
            "Type 4 function must be enclosed in { }".into(),
        ));
    }
    let inner = &s[1..s.len() - 1];
    // Outermost `{ ... }` consumes one depth slot.
    parse_body(inner, 1)
}

fn parse_body(s: &str, depth: u32) -> Result<Vec<Instruction>> {
    if depth > MAX_PARSE_DEPTH {
        return Err(Error::InvalidPdf(format!(
            "Type 4 parse depth limit exceeded (max {MAX_PARSE_DEPTH})"
        )));
    }
    let mut instructions = Vec::new();
    let mut chars = s.char_indices().peekable();

    while let Some(&(i, c)) = chars.peek() {
        if c.is_whitespace() {
            chars.next();
            continue;
        }
        if c == '{' {
            chars.next();
            let start = if let Some(&(idx, _)) = chars.peek() {
                idx
            } else {
                return Err(Error::InvalidPdf(
                    "Unclosed brace in Type 4 function".into(),
                ));
            };
            let mut brace_depth = 1u32;
            let mut end = start;
            for (j, ch) in chars.by_ref() {
                if ch == '{' {
                    brace_depth += 1;
                } else if ch == '}' {
                    brace_depth -= 1;
                    if brace_depth == 0 {
                        end = j;
                        break;
                    }
                }
            }
            if brace_depth != 0 {
                return Err(Error::InvalidPdf(
                    "Unclosed brace in Type 4 function".into(),
                ));
            }
            let body = parse_body(&s[start..end], depth + 1)?;
            instructions.push(Instruction::ProcedureBody(body));
            continue;
        }
        // Collect a token
        let start = i;
        while let Some(&(_, tc)) = chars.peek() {
            if tc.is_whitespace() || tc == '{' || tc == '}' {
                break;
            }
            chars.next();
        }
        let end = if let Some(&(idx, _)) = chars.peek() {
            idx
        } else {
            s.len()
        };
        let token = &s[start..end];
        instructions.push(parse_token(token)?);
    }

    // Post-process: resolve `if` and `ifelse` by consuming preceding procedure bodies.
    resolve_conditionals(&mut instructions)?;
    Ok(instructions)
}

fn parse_token(token: &str) -> Result<Instruction> {
    match token {
        "add" => Ok(Instruction::Add),
        "sub" => Ok(Instruction::Sub),
        "mul" => Ok(Instruction::Mul),
        "div" => Ok(Instruction::Div),
        "idiv" => Ok(Instruction::Idiv),
        "mod" => Ok(Instruction::Mod),
        "neg" => Ok(Instruction::Neg),
        "abs" => Ok(Instruction::Abs),
        "ceiling" => Ok(Instruction::Ceiling),
        "floor" => Ok(Instruction::Floor),
        "round" => Ok(Instruction::Round),
        "truncate" => Ok(Instruction::Truncate),
        "sqrt" => Ok(Instruction::Sqrt),
        "exp" => Ok(Instruction::Exp),
        "ln" => Ok(Instruction::Ln),
        "log" => Ok(Instruction::Log),
        "sin" => Ok(Instruction::Sin),
        "cos" => Ok(Instruction::Cos),
        "atan" => Ok(Instruction::Atan),
        "eq" => Ok(Instruction::Eq),
        "ne" => Ok(Instruction::Ne),
        "gt" => Ok(Instruction::Gt),
        "ge" => Ok(Instruction::Ge),
        "lt" => Ok(Instruction::Lt),
        "le" => Ok(Instruction::Le),
        "and" => Ok(Instruction::And),
        "or" => Ok(Instruction::Or),
        "xor" => Ok(Instruction::Xor),
        "not" => Ok(Instruction::Not),
        "bitshift" => Ok(Instruction::Bitshift),
        "cvi" => Ok(Instruction::Cvi),
        "cvr" => Ok(Instruction::Cvr),
        "true" => Ok(Instruction::BoolLiteral(true)),
        "false" => Ok(Instruction::BoolLiteral(false)),
        "dup" => Ok(Instruction::Dup),
        "exch" => Ok(Instruction::Exch),
        "pop" => Ok(Instruction::Pop),
        "copy" => Ok(Instruction::Copy),
        "index" => Ok(Instruction::Index),
        "roll" => Ok(Instruction::Roll),
        "if" | "ifelse" => Ok(if token == "if" {
            // Placeholder; resolved in post-processing
            Instruction::If(vec![])
        } else {
            Instruction::IfElse(vec![], vec![])
        }),
        _ => parse_numeric_literal(token),
    }
}

/// Parse a numeric literal. PLRM §3.3.2 specifies decimal/real syntax only;
/// `inf`, `NaN`, hex, and radix forms are not part of the Type 4 subset
/// (ISO 32000-1 Table 42). Reject anything that round-trips to a non-finite
/// f64 so malformed streams cannot smuggle in poisoned values.
fn parse_numeric_literal(token: &str) -> Result<Instruction> {
    // Prefer an integer parse so `52 not` and similar stay typed as integers.
    if let Ok(i) = token.parse::<i64>() {
        return Ok(Instruction::IntLiteral(i));
    }
    let val: f64 = token
        .parse()
        .map_err(|_| Error::InvalidPdf(format!("Unknown Type 4 token: {token}")))?;
    if !val.is_finite() {
        return Err(Error::InvalidPdf(format!(
            "Type 4 numeric literal must be finite, got: {token}"
        )));
    }
    Ok(Instruction::NumberLiteral(val))
}

/// Post-process: attach preceding procedure bodies to `if`/`ifelse` and reject
/// orphan procedure bodies that aren't consumed by a conditional.
fn resolve_conditionals(instructions: &mut Vec<Instruction>) -> Result<()> {
    let mut i = 0;
    while i < instructions.len() {
        match &instructions[i] {
            Instruction::If(body) if body.is_empty() => {
                // `if`: one preceding procedure body
                if i == 0 {
                    return Err(Error::InvalidPdf(
                        "Type 4 `if` without preceding procedure body".into(),
                    ));
                }
                match instructions.remove(i - 1) {
                    Instruction::ProcedureBody(body) => {
                        instructions[i - 1] = Instruction::If(body);
                        // Don't increment i; we removed an element before
                    }
                    _ => {
                        return Err(Error::InvalidPdf(
                            "Type 4 `if` requires a procedure body".into(),
                        ));
                    }
                }
            }
            Instruction::IfElse(true_b, false_b) if true_b.is_empty() && false_b.is_empty() => {
                // `ifelse`: two preceding procedure bodies
                if i < 2 {
                    return Err(Error::InvalidPdf(
                        "Type 4 `ifelse` without two preceding procedure bodies".into(),
                    ));
                }
                let false_branch = match instructions.remove(i - 1) {
                    Instruction::ProcedureBody(body) => body,
                    _ => {
                        return Err(Error::InvalidPdf(
                            "Type 4 `ifelse` requires two procedure bodies".into(),
                        ))
                    }
                };
                let true_branch = match instructions.remove(i - 2) {
                    Instruction::ProcedureBody(body) => body,
                    _ => {
                        return Err(Error::InvalidPdf(
                            "Type 4 `ifelse` requires two procedure bodies".into(),
                        ))
                    }
                };
                instructions[i - 2] = Instruction::IfElse(true_branch, false_branch);
                i = i.saturating_sub(1);
            }
            _ => {
                i += 1;
            }
        }
    }
    // Any procedure body that survives the resolve pass is an orphan — a
    // `{ ... }` block not followed by `if`/`ifelse`. PLRM has no concept of
    // executing a procedure object directly from this subset; reject it.
    if instructions
        .iter()
        .any(|ins| matches!(ins, Instruction::ProcedureBody(_)))
    {
        return Err(Error::InvalidPdf(
            "Type 4 orphan procedure body: { ... } not consumed by if/ifelse".into(),
        ));
    }
    Ok(())
}
