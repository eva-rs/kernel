//! Формулы Plural-Forms исполняются с ограниченной глубиной и проверяемой арифметикой.

#[derive(Debug)]
enum Expr {
    N,
    Num(i128),
    Unary(u8, Box<Expr>),
    Binary(String, Box<Expr>, Box<Expr>),
    Choose(Box<Expr>, Box<Expr>, Box<Expr>),
}

pub(super) fn evaluate(formula: &str, n: u64) -> Result<i128, String> {
    if formula.len() > 4096 {
        return Err("plural expression is too long".into());
    }
    let mut parser = Parser {
        text: formula.as_bytes(),
        at: 0,
        nodes: 0,
    };
    let expr = parser.expression(0, 0)?;
    parser.space();
    if parser.at != parser.text.len() {
        return Err("trailing tokens in plural expression".into());
    }
    eval(&expr, i128::from(n))
}

struct Parser<'a> {
    text: &'a [u8],
    at: usize,
    nodes: usize,
}
impl Parser<'_> {
    fn space(&mut self) {
        while self.text.get(self.at).is_some_and(u8::is_ascii_whitespace) {
            self.at += 1;
        }
    }
    fn eat(&mut self, token: &[u8]) -> bool {
        self.space();
        if self.text[self.at..].starts_with(token) {
            self.at += token.len();
            true
        } else {
            false
        }
    }
    fn expression(&mut self, min: u8, depth: usize) -> Result<Expr, String> {
        self.nodes += 1;
        if depth > 32 || self.nodes > 256 {
            return Err("plural expression is too complex".into());
        }
        self.space();
        let mut left = if self.eat(b"(") {
            let e = self.expression(0, depth + 1)?;
            if !self.eat(b")") {
                return Err("unclosed plural expression".into());
            }
            e
        } else if self.eat(b"!") {
            Expr::Unary(b'!', Box::new(self.expression(7, depth + 1)?))
        } else if self.eat(b"-") {
            Expr::Unary(b'-', Box::new(self.expression(7, depth + 1)?))
        } else if self.eat(b"+") {
            self.expression(7, depth + 1)?
        } else if self.eat(b"n") {
            Expr::N
        } else {
            let start = self.at;
            while self.text.get(self.at).is_some_and(u8::is_ascii_digit) {
                self.at += 1;
            }
            let num = std::str::from_utf8(&self.text[start..self.at])
                .ok()
                .and_then(|s| s.parse().ok())
                .ok_or_else(|| "expected number or n in plural expression".to_owned())?;
            Expr::Num(num)
        };
        loop {
            self.space();
            let operator = [
                ("||", 1),
                ("&&", 2),
                ("==", 3),
                ("!=", 3),
                ("<=", 4),
                (">=", 4),
                ("<", 4),
                (">", 4),
                ("+", 5),
                ("-", 5),
                ("*", 6),
                ("/", 6),
                ("%", 6),
            ]
            .into_iter()
            .find(|(op, _)| self.text[self.at..].starts_with(op.as_bytes()));
            let Some((op, prec)) = operator else {
                break;
            };
            if prec < min {
                break;
            }
            self.at += op.len();
            let right = self.expression(prec + 1, depth + 1)?;
            left = Expr::Binary(op.into(), Box::new(left), Box::new(right));
        }
        if min == 0 && self.eat(b"?") {
            let yes = self.expression(0, depth + 1)?;
            if !self.eat(b":") {
                return Err("missing ':' in plural expression".into());
            }
            let no = self.expression(0, depth + 1)?;
            left = Expr::Choose(Box::new(left), Box::new(yes), Box::new(no));
        }
        Ok(left)
    }
}

fn eval(e: &Expr, n: i128) -> Result<i128, String> {
    let invalid = || "invalid arithmetic in plural expression".to_owned();
    Ok(match e {
        Expr::N => n,
        Expr::Num(x) => *x,
        Expr::Unary(b'!', e) => i128::from(eval(e, n)? == 0),
        Expr::Unary(_, e) => eval(e, n)?.checked_neg().ok_or_else(invalid)?,
        Expr::Choose(c, yes, no) => eval(if eval(c, n)? != 0 { yes } else { no }, n)?,
        Expr::Binary(op, a, b) => {
            let a = eval(a, n)?;
            if op == "&&" && a == 0 {
                return Ok(0);
            }
            if op == "||" && a != 0 {
                return Ok(1);
            }
            let b = eval(b, n)?;
            match op.as_str() {
                "&&" | "||" => i128::from(b != 0),
                "==" => i128::from(a == b),
                "!=" => i128::from(a != b),
                "<" => i128::from(a < b),
                "<=" => i128::from(a <= b),
                ">" => i128::from(a > b),
                ">=" => i128::from(a >= b),
                "+" => a.checked_add(b).ok_or_else(invalid)?,
                "-" => a.checked_sub(b).ok_or_else(invalid)?,
                "*" => a.checked_mul(b).ok_or_else(invalid)?,
                "/" => a.checked_div(b).ok_or_else(invalid)?,
                "%" => a.checked_rem(b).ok_or_else(invalid)?,
                _ => return Err("unknown plural operator".into()),
            }
        }
    })
}
