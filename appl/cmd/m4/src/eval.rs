//! `eval`: 64-bit integer expressions with Limbo's operators and
//! precedence. Literals are decimal integers only; `**` is power and
//! right-associative; `>>` is an arithmetic shift.

/// Why an expression failed: the text consumed so far and the rest, for a
/// syntax error, or a bad operand (zero divide, negative power, an
/// oversized shift or power).
#[derive(Debug, PartialEq, Eq)]
pub enum EvalError {
    Syntax(String, String),
    Bad,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Tok {
    Eof,
    Ge,
    Le,
    Ne,
    Eq,
    Pow,
    And,
    Or,
    Rsh,
    Lsh,
    Num(i64),
    Char(char),
}

struct Lexer<'a> {
    s: &'a [char],
    p: usize,
    looked: Option<Tok>,
}

fn isspace(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n' | '\r')
}

impl Lexer<'_> {
    fn next_is(&mut self, c: char) -> bool {
        if self.s.get(self.p) == Some(&c) {
            self.p += 1;
            true
        } else {
            false
        }
    }

    fn lex(&mut self) -> Tok {
        if let Some(t) = self.looked.take() {
            return t;
        }
        while self.p < self.s.len() && isspace(self.s[self.p]) {
            self.p += 1;
        }
        let Some(&c) = self.s.get(self.p) else {
            return Tok::Eof;
        };
        self.p += 1;
        match c {
            '*' if self.next_is('*') => Tok::Pow,
            '>' if self.next_is('=') => Tok::Ge,
            '>' if self.next_is('>') => Tok::Rsh,
            '<' if self.next_is('=') => Tok::Le,
            '<' if self.next_is('<') => Tok::Lsh,
            '=' => {
                self.next_is('=');
                Tok::Eq
            }
            '!' if self.next_is('=') => Tok::Ne,
            '|' if self.next_is('|') => Tok::Or,
            '&' if self.next_is('&') => Tok::And,
            '0'..='9' => {
                let mut n: i64 = i64::from(c as u8 - b'0');
                while let Some(&d @ '0'..='9') = self.s.get(self.p) {
                    n = n.wrapping_mul(10).wrapping_add(i64::from(d as u8 - b'0'));
                    self.p += 1;
                }
                Tok::Num(n)
            }
            c => Tok::Char(c),
        }
    }

    fn look(&mut self) -> Tok {
        let t = self.lex();
        self.looked = Some(t);
        t
    }

    fn syntax(&self) -> EvalError {
        EvalError::Syntax(
            self.s[..self.p].iter().collect(),
            self.s[self.p..].iter().collect(),
        )
    }
}

fn priority(t: Tok) -> u32 {
    match t {
        Tok::Or => 1,
        Tok::And => 2,
        Tok::Char('|') => 3,
        Tok::Char('^') => 4,
        Tok::Char('&') => 5,
        Tok::Eq | Tok::Ne => 6,
        Tok::Char('<' | '>') | Tok::Ge | Tok::Le => 7,
        Tok::Lsh | Tok::Rsh => 8,
        Tok::Char('+' | '-') => 9,
        Tok::Char('*' | '/' | '%') => 10,
        Tok::Pow => 11,
        _ => 0,
    }
}

/// A shift count or exponent: Limbo's `int`, so at most 2^31-1.
fn intof(v: i64) -> Result<i32, EvalError> {
    if v > i64::from(i32::MAX) {
        return Err(EvalError::Bad);
    }
    Ok(v as i32)
}

fn apply(op: Tok, a: i64, b: i64) -> Result<i64, EvalError> {
    Ok(match op {
        Tok::Char('+') => a.wrapping_add(b),
        Tok::Char('-') => a.wrapping_sub(b),
        Tok::Char('*') => a.wrapping_mul(b),
        Tok::Char('/' | '%') if b == 0 => return Err(EvalError::Bad),
        Tok::Char('/') => a.wrapping_div(b),
        Tok::Char('%') => a.wrapping_rem(b),
        Tok::Pow if b < 0 => return Err(EvalError::Bad),
        Tok::Pow => a.wrapping_pow(intof(b)? as u32),
        Tok::Char('&') => a & b,
        Tok::Char('|') => a | b,
        Tok::Char('^') => a ^ b,
        Tok::Lsh => a.wrapping_shl(intof(b)? as u32),
        Tok::Rsh => a.wrapping_shr(intof(b)? as u32),
        Tok::And => i64::from(a != 0 && b != 0),
        Tok::Or => i64::from(a != 0 || b != 0),
        Tok::Char('<') => i64::from(a < b),
        Tok::Char('>') => i64::from(a > b),
        Tok::Le => i64::from(a <= b),
        Tok::Ge => i64::from(a >= b),
        Tok::Ne => i64::from(a != b),
        Tok::Eq => i64::from(a == b),
        _ => return Err(EvalError::Bad),
    })
}

fn expr(lx: &mut Lexer, prec: u32) -> Result<i64, EvalError> {
    let mut v = primary(lx)?;
    while priority(lx.look()) >= prec {
        let op = lx.lex();
        let next = priority(op) + u32::from(op != Tok::Pow);
        let r = expr(lx, next)?;
        v = apply(op, v, r)?;
    }
    Ok(v)
}

fn primary(lx: &mut Lexer) -> Result<i64, EvalError> {
    match lx.lex() {
        Tok::Char('(') => {
            let v = expr(lx, 1)?;
            if lx.lex() != Tok::Char(')') {
                return Err(lx.syntax());
            }
            Ok(v)
        }
        Tok::Char('+') => primary(lx),
        Tok::Char('-') => Ok(primary(lx)?.wrapping_neg()),
        Tok::Char('!') => Ok(i64::from(primary(lx)? == 0)),
        Tok::Char('~') => Ok(!primary(lx)?),
        Tok::Num(n) => Ok(n),
        _ => Err(lx.syntax()),
    }
}

/// Evaluates `s`.
pub fn eval(s: &str) -> Result<i64, EvalError> {
    let chars: Vec<char> = s.chars().collect();
    let mut lx = Lexer {
        s: &chars,
        p: 0,
        looked: None,
    };
    let v = expr(&mut lx, 1)?;
    if lx.p < chars.len() {
        return Err(lx.syntax());
    }
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::{EvalError, eval};

    fn ok(s: &str) -> i64 {
        eval(s).unwrap_or_else(|e| panic!("{s}: {e:?}"))
    }

    #[test]
    fn arithmetic_and_precedence() {
        assert_eq!(ok("1+2*3"), 7);
        assert_eq!(ok("(1+2)*3"), 9);
        assert_eq!(ok(" 7 / 2 "), 3);
        assert_eq!(ok("-7 % 3"), -1);
        assert_eq!(ok("2**3**2"), 512);
        assert_eq!(ok("-2**2"), 4);
        assert_eq!(ok("1<<4|1"), 17);
        assert_eq!(ok("-16>>2"), -4);
        assert_eq!(ok("6&3^1"), 3);
        assert_eq!(ok("1+1==2&&3>2"), 1);
        assert_eq!(ok("0||!0"), 1);
        assert_eq!(ok("~0"), -1);
        assert_eq!(ok("+-+5"), -5);
        assert_eq!(ok("3 = 3"), 1);
        assert_eq!(ok("2 != 2"), 0);
        assert_eq!(ok("2 <= 2 >= 1"), 1);
        assert_eq!(ok("9223372036854775807+1"), i64::MIN);
    }

    #[test]
    fn errors() {
        assert_eq!(eval("1/0"), Err(EvalError::Bad));
        assert_eq!(eval("5%0"), Err(EvalError::Bad));
        assert_eq!(eval("2**-1"), Err(EvalError::Bad));
        assert_eq!(eval("1<<4294967296"), Err(EvalError::Bad));
        assert_eq!(eval("1+"), Err(EvalError::Syntax("1+".into(), "".into())));
        assert_eq!(
            eval("(1 2"),
            Err(EvalError::Syntax("(1 2".into(), "".into()))
        );
        assert_eq!(eval("x"), Err(EvalError::Syntax("x".into(), "".into())));
        assert_eq!(
            eval("1 2 3"),
            Err(EvalError::Syntax("1 2".into(), " 3".into()))
        );
    }

    #[test]
    fn trailing_token_is_dropped() {
        // The look-ahead that ends an expression consumes its token, so a
        // single trailing operator-less token is not an error.
        assert_eq!(ok("1 x"), 1);
        assert_eq!(ok("4)"), 4);
    }
}
