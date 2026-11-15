//! サンドボックス化された純粋 Rust 数式パーサ＆評価器（`AnimatedParameter::Expression` 用）。
//!
//! 外部実行権限・I/O・ループ・再帰展開を一切持たず、制限付き AST（最大 4096 バイト・最大 256 ノード・深さ 64）として安全に評価する。

use thiserror::Error;

const MAX_EXPRESSION_BYTES: usize = 4096;
const MAX_AST_NODES: usize = 256;
const MAX_RECURSION_DEPTH: usize = 64;

/// 数式評価時に渡す時刻コンテキスト。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ExprContext {
    /// エフェクト基準原点からの経過秒（`effect_frame / fps`）。
    pub t: f64,
    /// 正規化進行率（通常 `0.0..=1.0`、カット後も元の尺基準で維持）。
    pub p: f64,
    /// エフェクト全体の基準長（秒）。
    pub d: f64,
}

/// 数式の字句解析・構文解析・評価で発生し得るエラー。
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ExpressionError {
    /// 空または空白のみの数式が渡された。
    #[error("expression cannot be empty")]
    Empty,
    /// 数式文字列が最大長を超えている。
    #[error("expression exceeds maximum length ({MAX_EXPRESSION_BYTES} bytes)")]
    TooLong,
    /// 解釈できない文字が含まれている。
    #[error("unexpected character `{0}` in expression")]
    UnexpectedChar(char),
    /// 予期しないトークンが現れた。
    #[error("unexpected token `{0}` in expression")]
    UnexpectedToken(String),
    /// 式が途中で終わっている。
    #[error("unexpected end of expression")]
    UnexpectedEnd,
    /// AST ノード数またはネスト深さが制限を超えた。
    #[error("expression exceeds complexity limits")]
    TooComplex,
    /// 定義されていない変数名が参照された。
    #[error("unknown identifier `{0}`")]
    UnknownIdentifier(String),
    /// 定義されていない関数名が呼び出された。
    #[error("unknown function `{0}`")]
    UnknownFunction(String),
    /// 関数引数の個数が誤っている。
    #[error("function `{name}` expects {expected} argument(s), got {found}")]
    WrongArity {
        /// 関数名。
        name: String,
        /// 期待される引数個数。
        expected: usize,
        /// 実際の引数個数。
        found: usize,
    },
    /// 0 除算または 0 剰余が発生した。
    #[error("division by zero in expression")]
    DivisionByZero,
    /// 計算結果が NaN または無限大になった。
    #[error("expression produced non-finite result")]
    NonFiniteResult,
}

#[derive(Debug, Clone, PartialEq)]
enum Token {
    Number(f64),
    Ident(String),
    Plus,
    Minus,
    Star,
    Slash,
    Percent,
    Caret,
    LParen,
    RParen,
    Comma,
}

#[derive(Debug, Clone, Copy)]
enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    Rem,
    Pow,
}

#[derive(Debug, Clone, Copy)]
enum UnaryFn {
    Sin,
    Cos,
    Tan,
    Abs,
    Sqrt,
    Floor,
    Ceil,
    Round,
}

#[derive(Debug, Clone, Copy)]
enum BinaryFn {
    Min,
    Max,
    Pow,
}

#[derive(Debug, Clone, Copy)]
enum TernaryFn {
    Clamp,
    Lerp,
    Smoothstep,
}

#[derive(Debug, Clone, Copy)]
enum ExprNode {
    Literal(f64),
    VarT,
    VarP,
    VarD,
    Neg(usize),
    BinOp(BinOp, usize, usize),
    Call1(UnaryFn, usize),
    Call2(BinaryFn, usize, usize),
    Call3(TernaryFn, usize, usize, usize),
}

/// 事前パース済みで繰り返し評価可能な安全な AST 式。
#[derive(Debug, Clone)]
pub struct CompiledExpression {
    nodes: Vec<ExprNode>,
    root: usize,
}

impl CompiledExpression {
    /// 数式文字列を字句・構文解析し、評価可能な AST へコンパイルする。
    ///
    /// # Errors
    ///
    /// 構文エラー、未知の識別子・関数、またはサイズ上限超過時に [`ExpressionError`] を返す。
    pub fn parse(input: &str) -> Result<Self, ExpressionError> {
        if input.len() > MAX_EXPRESSION_BYTES {
            return Err(ExpressionError::TooLong);
        }
        let trimmed = input.trim();
        if trimmed.is_empty() {
            return Err(ExpressionError::Empty);
        }

        let tokens = tokenize(trimmed)?;
        let mut parser = Parser {
            tokens: &tokens,
            pos: 0,
            nodes: Vec::with_capacity(tokens.len()),
        };
        let root = parser.parse_additive(0)?;
        if let Some(leftover) = parser.peek() {
            return Err(ExpressionError::UnexpectedToken(format!("{leftover:?}")));
        }

        Ok(Self {
            nodes: parser.nodes,
            root,
        })
    }

    /// コンパイル済み AST を指定コンテキストで評価する。
    ///
    /// # Errors
    ///
    /// 0 除算・負数の平方根・NaN/Infinity 発生時に [`ExpressionError`] を返す。
    pub fn evaluate(&self, ctx: ExprContext) -> Result<f64, ExpressionError> {
        let value = self.eval_node(self.root, ctx)?;
        if value.is_finite() {
            Ok(value)
        } else {
            Err(ExpressionError::NonFiniteResult)
        }
    }

    fn eval_node(&self, idx: usize, ctx: ExprContext) -> Result<f64, ExpressionError> {
        let val = match self.nodes[idx] {
            ExprNode::Literal(v) => v,
            ExprNode::VarT => ctx.t,
            ExprNode::VarP => ctx.p,
            ExprNode::VarD => ctx.d,
            ExprNode::Neg(inner) => -self.eval_node(inner, ctx)?,
            ExprNode::BinOp(op, lhs, rhs) => {
                let a = self.eval_node(lhs, ctx)?;
                let b = self.eval_node(rhs, ctx)?;
                eval_bin_op(op, a, b)?
            }
            ExprNode::Call1(func, arg0) => {
                let x = self.eval_node(arg0, ctx)?;
                eval_unary_fn(func, x)?
            }
            ExprNode::Call2(func, arg0, arg1) => {
                let a = self.eval_node(arg0, ctx)?;
                let b = self.eval_node(arg1, ctx)?;
                eval_binary_fn(func, a, b)
            }
            ExprNode::Call3(func, arg0, arg1, arg2) => {
                let a = self.eval_node(arg0, ctx)?;
                let b = self.eval_node(arg1, ctx)?;
                let c = self.eval_node(arg2, ctx)?;
                eval_ternary_fn(func, a, b, c)
            }
        };
        if val.is_finite() {
            Ok(val)
        } else {
            Err(ExpressionError::NonFiniteResult)
        }
    }
}

/// 文字列数式を 1 回だけ解析して即座に評価するヘルパー。
///
/// # Errors
///
/// 構文エラーまたは評価時の数学エラーが発生した場合 [`ExpressionError`] を返す。
pub fn evaluate_expression(input: &str, ctx: ExprContext) -> Result<f64, ExpressionError> {
    CompiledExpression::parse(input)?.evaluate(ctx)
}

fn eval_bin_op(op: BinOp, a: f64, b: f64) -> Result<f64, ExpressionError> {
    match op {
        BinOp::Add => Ok(a + b),
        BinOp::Sub => Ok(a - b),
        BinOp::Mul => Ok(a * b),
        BinOp::Div => {
            if b == 0.0 {
                Err(ExpressionError::DivisionByZero)
            } else {
                Ok(a / b)
            }
        }
        BinOp::Rem => {
            if b == 0.0 {
                Err(ExpressionError::DivisionByZero)
            } else {
                Ok(a % b)
            }
        }
        BinOp::Pow => Ok(a.powf(b)),
    }
}

fn eval_unary_fn(func: UnaryFn, x: f64) -> Result<f64, ExpressionError> {
    match func {
        UnaryFn::Sin => Ok(x.sin()),
        UnaryFn::Cos => Ok(x.cos()),
        UnaryFn::Tan => Ok(x.tan()),
        UnaryFn::Abs => Ok(x.abs()),
        UnaryFn::Sqrt => {
            if x < 0.0 {
                Err(ExpressionError::NonFiniteResult)
            } else {
                Ok(x.sqrt())
            }
        }
        UnaryFn::Floor => Ok(x.floor()),
        UnaryFn::Ceil => Ok(x.ceil()),
        UnaryFn::Round => Ok(x.round()),
    }
}

fn eval_binary_fn(func: BinaryFn, a: f64, b: f64) -> f64 {
    match func {
        BinaryFn::Min => a.min(b),
        BinaryFn::Max => a.max(b),
        BinaryFn::Pow => a.powf(b),
    }
}

fn eval_ternary_fn(func: TernaryFn, a: f64, b: f64, c: f64) -> f64 {
    match func {
        TernaryFn::Clamp => {
            let lo = b.min(c);
            let hi = b.max(c);
            a.clamp(lo, hi)
        }
        TernaryFn::Lerp => (b - a).mul_add(c, a),
        TernaryFn::Smoothstep => {
            let span = b - a;
            if span.abs() <= f64::EPSILON {
                if c >= b { 1.0 } else { 0.0 }
            } else {
                let t = ((c - a) / span).clamp(0.0, 1.0);
                t * t * 2.0f64.mul_add(-t, 3.0)
            }
        }
    }
}

struct Parser<'tok> {
    tokens: &'tok [Token],
    pos: usize,
    nodes: Vec<ExprNode>,
}

impl Parser<'_> {
    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.pos)
    }

    fn bump(&mut self) -> Option<&Token> {
        let tok = self.tokens.get(self.pos);
        if tok.is_some() {
            self.pos += 1;
        }
        tok
    }

    fn push_node(&mut self, node: ExprNode) -> Result<usize, ExpressionError> {
        if self.nodes.len() >= MAX_AST_NODES {
            return Err(ExpressionError::TooComplex);
        }
        let idx = self.nodes.len();
        self.nodes.push(node);
        Ok(idx)
    }

    fn parse_additive(&mut self, depth: usize) -> Result<usize, ExpressionError> {
        check_depth(depth)?;
        let mut lhs = self.parse_multiplicative(depth + 1)?;
        while let Some(op) = match self.peek() {
            Some(Token::Plus) => Some(BinOp::Add),
            Some(Token::Minus) => Some(BinOp::Sub),
            _ => None,
        } {
            self.bump();
            let rhs = self.parse_multiplicative(depth + 1)?;
            lhs = self.push_node(ExprNode::BinOp(op, lhs, rhs))?;
        }
        Ok(lhs)
    }

    fn parse_multiplicative(&mut self, depth: usize) -> Result<usize, ExpressionError> {
        check_depth(depth)?;
        let mut lhs = self.parse_power(depth + 1)?;
        while let Some(op) = match self.peek() {
            Some(Token::Star) => Some(BinOp::Mul),
            Some(Token::Slash) => Some(BinOp::Div),
            Some(Token::Percent) => Some(BinOp::Rem),
            _ => None,
        } {
            self.bump();
            let rhs = self.parse_power(depth + 1)?;
            lhs = self.push_node(ExprNode::BinOp(op, lhs, rhs))?;
        }
        Ok(lhs)
    }

    fn parse_power(&mut self, depth: usize) -> Result<usize, ExpressionError> {
        check_depth(depth)?;
        let lhs = self.parse_unary(depth + 1)?;
        if matches!(self.peek(), Some(Token::Caret)) {
            self.bump();
            let rhs = self.parse_power(depth + 1)?;
            return self.push_node(ExprNode::BinOp(BinOp::Pow, lhs, rhs));
        }
        Ok(lhs)
    }

    fn parse_unary(&mut self, depth: usize) -> Result<usize, ExpressionError> {
        check_depth(depth)?;
        match self.peek() {
            Some(Token::Plus) => {
                self.bump();
                self.parse_unary(depth + 1)
            }
            Some(Token::Minus) => {
                self.bump();
                let inner = self.parse_unary(depth + 1)?;
                self.push_node(ExprNode::Neg(inner))
            }
            _ => self.parse_primary(depth + 1),
        }
    }

    fn parse_primary(&mut self, depth: usize) -> Result<usize, ExpressionError> {
        check_depth(depth)?;
        let token = self.bump().cloned().ok_or(ExpressionError::UnexpectedEnd)?;
        match token {
            Token::Number(value) => self.push_node(ExprNode::Literal(value)),
            Token::LParen => {
                let inner = self.parse_additive(depth + 1)?;
                if !matches!(self.bump(), Some(Token::RParen)) {
                    return Err(ExpressionError::UnexpectedToken("expected `)`".to_owned()));
                }
                Ok(inner)
            }
            Token::Ident(name) => {
                if matches!(self.peek(), Some(Token::LParen)) {
                    self.parse_function_call(&name, depth + 1)
                } else {
                    self.resolve_identifier(&name)
                }
            }
            other => Err(ExpressionError::UnexpectedToken(format!("{other:?}"))),
        }
    }

    fn resolve_identifier(&mut self, name: &str) -> Result<usize, ExpressionError> {
        let node = match name {
            "t" => ExprNode::VarT,
            "p" => ExprNode::VarP,
            "d" => ExprNode::VarD,
            "pi" => ExprNode::Literal(std::f64::consts::PI),
            "e" => ExprNode::Literal(std::f64::consts::E),
            _ => return Err(ExpressionError::UnknownIdentifier(name.to_owned())),
        };
        self.push_node(node)
    }

    fn parse_function_call(&mut self, name: &str, depth: usize) -> Result<usize, ExpressionError> {
        self.bump();
        let mut args = Vec::new();
        if !matches!(self.peek(), Some(Token::RParen)) {
            loop {
                args.push(self.parse_additive(depth + 1)?);
                match self.peek() {
                    Some(Token::Comma) => {
                        self.bump();
                    }
                    Some(Token::RParen) => break,
                    Some(tok) => return Err(ExpressionError::UnexpectedToken(format!("{tok:?}"))),
                    None => return Err(ExpressionError::UnexpectedEnd),
                }
            }
        }
        self.bump();
        self.build_call_node(name, &args)
    }

    fn build_call_node(&mut self, name: &str, args: &[usize]) -> Result<usize, ExpressionError> {
        if let Some(unary) = match_unary_fn(name) {
            expect_arity(name, 1, args.len())?;
            return self.push_node(ExprNode::Call1(unary, args[0]));
        }
        if let Some(binary) = match_binary_fn(name) {
            expect_arity(name, 2, args.len())?;
            return self.push_node(ExprNode::Call2(binary, args[0], args[1]));
        }
        if let Some(ternary) = match_ternary_fn(name) {
            expect_arity(name, 3, args.len())?;
            return self.push_node(ExprNode::Call3(ternary, args[0], args[1], args[2]));
        }
        Err(ExpressionError::UnknownFunction(name.to_owned()))
    }
}

const fn check_depth(depth: usize) -> Result<(), ExpressionError> {
    if depth > MAX_RECURSION_DEPTH {
        Err(ExpressionError::TooComplex)
    } else {
        Ok(())
    }
}

fn expect_arity(name: &str, expected: usize, found: usize) -> Result<(), ExpressionError> {
    if expected == found {
        Ok(())
    } else {
        Err(ExpressionError::WrongArity {
            name: name.to_owned(),
            expected,
            found,
        })
    }
}

fn match_unary_fn(name: &str) -> Option<UnaryFn> {
    match name {
        "sin" => Some(UnaryFn::Sin),
        "cos" => Some(UnaryFn::Cos),
        "tan" => Some(UnaryFn::Tan),
        "abs" => Some(UnaryFn::Abs),
        "sqrt" => Some(UnaryFn::Sqrt),
        "floor" => Some(UnaryFn::Floor),
        "ceil" => Some(UnaryFn::Ceil),
        "round" => Some(UnaryFn::Round),
        _ => None,
    }
}

fn match_binary_fn(name: &str) -> Option<BinaryFn> {
    match name {
        "min" => Some(BinaryFn::Min),
        "max" => Some(BinaryFn::Max),
        "pow" => Some(BinaryFn::Pow),
        _ => None,
    }
}

fn match_ternary_fn(name: &str) -> Option<TernaryFn> {
    match name {
        "clamp" => Some(TernaryFn::Clamp),
        "lerp" => Some(TernaryFn::Lerp),
        "smoothstep" => Some(TernaryFn::Smoothstep),
        _ => None,
    }
}

fn tokenize(input: &str) -> Result<Vec<Token>, ExpressionError> {
    let bytes = input.as_bytes();
    let mut pos = 0;
    let mut tokens = Vec::new();

    while pos < bytes.len() {
        let ch = bytes[pos] as char;
        if ch.is_ascii_whitespace() {
            pos += 1;
            continue;
        }
        if let Some(simple) = single_char_token(ch) {
            tokens.push(simple);
            pos += 1;
            continue;
        }
        if ch.is_ascii_digit() || ch == '.' {
            let (number, next_pos) = read_number(input, pos)?;
            tokens.push(Token::Number(number));
            pos = next_pos;
            continue;
        }
        if ch.is_ascii_alphabetic() || ch == '_' {
            let start = pos;
            while pos < bytes.len() && (bytes[pos].is_ascii_alphanumeric() || bytes[pos] == b'_') {
                pos += 1;
            }
            tokens.push(Token::Ident(input[start..pos].to_owned()));
            continue;
        }
        let non_ascii = input[pos..].chars().next().unwrap_or(ch);
        return Err(ExpressionError::UnexpectedChar(non_ascii));
    }

    Ok(tokens)
}

const fn single_char_token(ch: char) -> Option<Token> {
    match ch {
        '+' => Some(Token::Plus),
        '-' => Some(Token::Minus),
        '*' => Some(Token::Star),
        '/' => Some(Token::Slash),
        '%' => Some(Token::Percent),
        '^' => Some(Token::Caret),
        '(' => Some(Token::LParen),
        ')' => Some(Token::RParen),
        ',' => Some(Token::Comma),
        _ => None,
    }
}

fn read_number(input: &str, start: usize) -> Result<(f64, usize), ExpressionError> {
    let bytes = input.as_bytes();
    let mut pos = start;
    let mut seen_dot = false;
    while pos < bytes.len() {
        let b = bytes[pos];
        if b.is_ascii_digit() {
            pos += 1;
        } else if b == b'.' && !seen_dot {
            seen_dot = true;
            pos += 1;
        } else {
            break;
        }
    }
    if pos < bytes.len() && (bytes[pos] == b'e' || bytes[pos] == b'E') {
        let exp_start = pos;
        pos += 1;
        if pos < bytes.len() && (bytes[pos] == b'+' || bytes[pos] == b'-') {
            pos += 1;
        }
        if pos >= bytes.len() || !bytes[pos].is_ascii_digit() {
            pos = exp_start;
        } else {
            while pos < bytes.len() && bytes[pos].is_ascii_digit() {
                pos += 1;
            }
        }
    }
    let slice = &input[start..pos];
    let parsed: f64 = slice
        .parse()
        .map_err(|_| ExpressionError::UnexpectedToken(slice.to_owned()))?;
    if !parsed.is_finite() {
        return Err(ExpressionError::NonFiniteResult);
    }
    Ok((parsed, pos))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expression_evaluates_math_precedence_and_functions() -> Result<(), Box<dyn std::error::Error>>
    {
        let ctx = ExprContext {
            t: 0.5,
            p: 0.25,
            d: 2.0,
        };
        let value = evaluate_expression("10 + sin(pi * t) * 100 + clamp(p, 0, 1) * d", ctx)?;
        assert!((value - 110.5).abs() < 1e-9);

        let power = evaluate_expression("2^3^2", ctx)?;
        assert!((power - 512.0).abs() < 1e-9);

        let smooth = evaluate_expression("smoothstep(0, 1, 0.5)", ctx)?;
        assert!((smooth - 0.5).abs() < 1e-9);
        Ok(())
    }

    #[test]
    fn expression_rejects_unknown_identifiers_and_division_by_zero() {
        let ctx = ExprContext {
            t: 0.0,
            p: 0.0,
            d: 1.0,
        };
        assert!(matches!(
            evaluate_expression("std::fs::read()", ctx),
            Err(ExpressionError::UnexpectedChar(':'))
        ));
        assert!(matches!(
            evaluate_expression("10 / t", ctx),
            Err(ExpressionError::DivisionByZero)
        ));
        assert!(matches!(
            evaluate_expression("sqrt(-1)", ctx),
            Err(ExpressionError::NonFiniteResult)
        ));
    }
}
