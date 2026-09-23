use dsqlex::evaluator::{Context, Value};
use rust_decimal::Decimal;
use std::str::FromStr;

const MAX_EXPR: usize = 8 * 1024;
const MAX_CTX: usize = 64 * 1024;
const MAX_DEPTH: usize = 8;
const MAX_KEYS: usize = 128;

fn ok(value: &Value) -> serde_json::Value {
    let ty = match value {
        Value::Decimal(_) => "decimal",
        Value::String(_) => "string",
        Value::Bool(_) => "boolean",
        Value::Null => "null",
    };
    serde_json::json!({ "status": "ok", "value": value.to_string(), "value_type": ty })
}

fn err(status: &str, message: impl Into<String>) -> serde_json::Value {
    serde_json::json!({ "status": status, "message": message.into() })
}

fn valid_key(k: &str) -> bool {
    let mut c = k.chars();
    matches!(c.next(), Some(ch) if ch.is_ascii_alphabetic() || ch == '_')
        && k.chars().all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
}

fn number_to_decimal(s: &str) -> Result<Decimal, String> {
    Decimal::from_str(s)
        .or_else(|_| Decimal::from_scientific(s))
        .map_err(|_| format!("unsupported or out-of-range number: {}", s))
}

fn json_into_ctx(
    map: &serde_json::Map<String, serde_json::Value>,
    ctx: &mut Context,
    path: &str,
    depth: usize,
    keys: &mut usize,
) -> Result<(), String> {
    if depth > MAX_DEPTH {
        return Err(format!(
            "{}: object nesting exceeds depth {}",
            path, MAX_DEPTH
        ));
    }
    for (k, v) in map {
        *keys += 1;
        if *keys > MAX_KEYS {
            return Err(format!("context exceeds {} keys", MAX_KEYS));
        }
        if !valid_key(k) {
            return Err(format!("{}: invalid identifier {:?}", path, k));
        }
        let p = if path.is_empty() {
            k.clone()
        } else {
            format!("{}.{}", path, k)
        };
        match v {
            serde_json::Value::Number(n) => {
                let d = number_to_decimal(&n.to_string()).map_err(|e| format!("{}: {}", p, e))?;
                ctx.set(k.as_str(), Value::Decimal(d));
            }
            serde_json::Value::String(s) => ctx.set_string(k.as_str(), s.as_str()),
            serde_json::Value::Bool(b) => ctx.set_bool(k.as_str(), *b),
            serde_json::Value::Null => ctx.set_null(k.as_str()),
            serde_json::Value::Object(m) => {
                let mut sub = Context::new();
                json_into_ctx(m, &mut sub, &p, depth + 1, keys)?;
                ctx.set_nested(k.as_str(), sub);
            }
            serde_json::Value::Array(_) => {
                return Err(format!("{}: arrays are not supported", p));
            }
        }
    }
    Ok(())
}

pub fn evaluate(expression: &str, context_json: &str) -> serde_json::Value {
    if expression.trim().is_empty() {
        return err("syntax_error", "expression is empty");
    }
    if expression.len() > MAX_EXPR {
        return err("syntax_error", "expression exceeds 8KiB limit");
    }
    if context_json.len() > MAX_CTX {
        return err("context_error", "context_json exceeds 64KiB limit");
    }
    let parsed: serde_json::Value = match serde_json::from_str(context_json) {
        Ok(v) => v,
        Err(e) => return err("context_error", format!("invalid JSON: {}", e)),
    };
    let map = match parsed {
        serde_json::Value::Object(m) => m,
        _ => return err("context_error", "context must be a JSON object"),
    };
    let mut ctx = Context::new();
    let mut keys = 0usize;
    if let Err(e) = json_into_ctx(&map, &mut ctx, "", 1, &mut keys) {
        return err("context_error", e);
    }
    let ast = match dsqlex::parse(expression) {
        Ok(a) => a,
        Err(e) => return err("syntax_error", e.to_string()),
    };
    match dsqlex::eval(&ast, &ctx) {
        Ok(v) => ok(&v),
        Err(e) => err("evaluation_error", e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CTX: &str = r#"{
  "country": "Brazil",
  "amount": 125.50,
  "quantity": 4,
  "customer_tier": "gold",
  "status": "active",
  "discount": null,
  "config": { "region": "LATAM", "pricing": { "margin": 0.15, "minimum": 25 } }
}"#;

    fn eval(expr: &str, ctx: &str) -> serde_json::Value {
        evaluate(expr, ctx)
    }

    #[test]
    fn default_case_decimal_scale_retained() {
        let r = eval(
            "CASE WHEN country = 'Brazil' THEN amount * 2 ELSE amount END",
            CTX,
        );
        assert_eq!(r["status"], "ok");
        assert_eq!(r["value"], "251.00");
        assert_eq!(r["value_type"], "decimal");
    }

    #[test]
    fn nested_round_and_coalesce() {
        let r = eval(
            "ROUND((amount * quantity) * (1 + config.pricing.margin), 2)",
            CTX,
        );
        assert_eq!(r["status"], "ok");
        assert_eq!(r["value"], "577.30");
        let r = eval("COALESCE(discount, 0)", CTX);
        assert_eq!(r["status"], "ok");
        assert_eq!(r["value"], "0");
    }

    #[test]
    fn error_kinds_are_distinguished() {
        assert_eq!(eval("CASE WHEN ((", CTX)["status"], "syntax_error");
        assert_eq!(eval("missing_field + 1", CTX)["status"], "evaluation_error");
        assert_eq!(eval("amount", "{bad json")["status"], "context_error");
        assert_eq!(eval("amount", "[1,2]")["status"], "context_error");
        assert_eq!(eval("x", r#"{"x": [1]}"#)["status"], "context_error");
        assert_eq!(eval("x", r#"{"x": 1e30}"#)["status"], "context_error");
        assert_eq!(eval("x", r#"{"bad key!": 1}"#)["status"], "context_error");
        assert_eq!(eval("", CTX)["status"], "syntax_error");
    }

    #[test]
    fn high_precision_and_scientific_numbers_exact() {
        let r = eval("rate", r#"{ "rate": 0.1234567890123456789012345678 }"#);
        assert_eq!(r["status"], "ok");
        assert_eq!(r["value"], "0.1234567890123456789012345678");
        let r = eval("x", r#"{ "x": 1e-3 }"#);
        assert_eq!(r["status"], "ok");
        assert_eq!(r["value"], "0.001");
    }

    #[test]
    fn string_bool_null_types() {
        let r = eval("country", CTX);
        assert_eq!(r["value_type"], "string");
        let r = eval("TRUE", CTX);
        assert_eq!(r["value_type"], "boolean");
        let r = eval("discount", CTX);
        assert_eq!(r["value_type"], "null");
        assert_eq!(r["value"], "NULL");
    }
}
