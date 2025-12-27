//! Lua Parser

use crate::lua::ast::*;
use crate::lua::lexer::Token;

/// Parse tokens into AST
pub fn parse(tokens: &[Token]) -> Result<Program, String> {
    let mut parser = Parser::new(tokens);
    parser.parse_program()
}

struct Parser<'a> {
    tokens: &'a [Token],
    pos: usize,
}

impl<'a> Parser<'a> {
    fn new(tokens: &'a [Token]) -> Self {
        Self { tokens, pos: 0 }
    }

    fn current(&self) -> &Token {
        self.tokens.get(self.pos).unwrap_or(&Token::Eof)
    }

    fn peek(&self, offset: usize) -> &Token {
        self.tokens.get(self.pos + offset).unwrap_or(&Token::Eof)
    }

    fn advance(&mut self) -> Token {
        let token = self.current().clone();
        self.pos += 1;
        token
    }

    fn expect(&mut self, expected: Token) -> Result<(), String> {
        if self.current() == &expected {
            self.advance();
            Ok(())
        } else {
            Err(format!(
                "Expected {:?}, found {:?}",
                expected,
                self.current()
            ))
        }
    }

    fn parse_program(&mut self) -> Result<Program, String> {
        let mut statements = Vec::new();

        while self.current() != &Token::Eof {
            statements.push(self.parse_statement()?);
        }

        Ok(Program { statements })
    }

    fn parse_statement(&mut self) -> Result<Stmt, String> {
        match self.current() {
            Token::Local => self.parse_local(),
            Token::If => self.parse_if(),
            Token::While => self.parse_while(),
            Token::Repeat => self.parse_repeat(),
            Token::For => self.parse_for(),
            Token::Function => self.parse_function(),
            Token::Return => self.parse_return(),
            Token::Do => self.parse_do(),
            Token::Goto => self.parse_goto(),
            Token::DoubleColon => self.parse_label(),
            Token::Break => {
                self.advance();
                Ok(Stmt::Break)
            }
            Token::Semicolon => {
                self.advance();
                self.parse_statement()
            }
            Token::Identifier(_) => self.parse_assignment_or_call(),
            _ => Err(format!("Unexpected token: {:?}", self.current())),
        }
    }

    fn parse_local(&mut self) -> Result<Stmt, String> {
        self.expect(Token::Local)?;

        // Check for local function
        if self.current() == &Token::Function {
            self.advance();
            let name = self.parse_identifier()?;
            return self.parse_function_body(FunctionName::simple(name), true);
        }

        // Variable declaration
        let mut names = Vec::new();
        names.push(self.parse_identifier()?);

        while self.current() == &Token::Comma {
            self.advance();
            names.push(self.parse_identifier()?);
        }

        let targets: Vec<AssignTarget> = names.into_iter().map(AssignTarget::Name).collect();

        let mut values = Vec::new();
        if self.current() == &Token::Assign {
            self.advance();
            values.push(self.parse_expression()?);

            while self.current() == &Token::Comma {
                self.advance();
                values.push(self.parse_expression()?);
            }
        }

        Ok(Stmt::Assign {
            targets,
            values,
            local: true,
        })
    }

    fn parse_if(&mut self) -> Result<Stmt, String> {
        self.expect(Token::If)?;
        let condition = self.parse_expression()?;
        self.expect(Token::Then)?;

        let then_block = self.parse_block(&[Token::Elseif, Token::Else, Token::End])?;

        let mut elseif_blocks = Vec::new();
        while self.current() == &Token::Elseif {
            self.advance();
            let cond = self.parse_expression()?;
            self.expect(Token::Then)?;
            let block = self.parse_block(&[Token::Elseif, Token::Else, Token::End])?;
            elseif_blocks.push((cond, block));
        }

        let else_block = if self.current() == &Token::Else {
            self.advance();
            Some(self.parse_block(&[Token::End])?)
        } else {
            None
        };

        self.expect(Token::End)?;

        Ok(Stmt::If {
            condition,
            then_block,
            elseif_blocks,
            else_block,
        })
    }

    fn parse_while(&mut self) -> Result<Stmt, String> {
        self.expect(Token::While)?;
        let condition = self.parse_expression()?;
        self.expect(Token::Do)?;
        let body = self.parse_block(&[Token::End])?;
        self.expect(Token::End)?;

        Ok(Stmt::While { condition, body })
    }

    fn parse_repeat(&mut self) -> Result<Stmt, String> {
        self.expect(Token::Repeat)?;
        let body = self.parse_block(&[Token::Until])?;
        self.expect(Token::Until)?;
        let condition = self.parse_expression()?;

        Ok(Stmt::Repeat { body, condition })
    }

    fn parse_for(&mut self) -> Result<Stmt, String> {
        self.expect(Token::For)?;
        let first_var = self.parse_identifier()?;

        if self.current() == &Token::Assign {
            // Numeric for
            self.advance();
            let start = self.parse_expression()?;
            self.expect(Token::Comma)?;
            let end = self.parse_expression()?;

            let step = if self.current() == &Token::Comma {
                self.advance();
                Some(self.parse_expression()?)
            } else {
                None
            };

            self.expect(Token::Do)?;
            let body = self.parse_block(&[Token::End])?;
            self.expect(Token::End)?;

            Ok(Stmt::ForNumeric {
                var: first_var,
                start,
                end,
                step,
                body,
            })
        } else {
            // Generic for
            let mut vars = vec![first_var];
            while self.current() == &Token::Comma {
                self.advance();
                vars.push(self.parse_identifier()?);
            }

            self.expect(Token::In)?;

            let mut exprs = vec![self.parse_expression()?];
            while self.current() == &Token::Comma {
                self.advance();
                exprs.push(self.parse_expression()?);
            }

            self.expect(Token::Do)?;
            let body = self.parse_block(&[Token::End])?;
            self.expect(Token::End)?;

            Ok(Stmt::ForGeneric { vars, exprs, body })
        }
    }

    fn parse_function(&mut self) -> Result<Stmt, String> {
        self.expect(Token::Function)?;
        let name = self.parse_function_name()?;
        self.parse_function_body(name, false)
    }

    fn parse_function_name(&mut self) -> Result<FunctionName, String> {
        let base = self.parse_identifier()?;
        let mut fields = Vec::new();
        let mut method = None;

        loop {
            if self.current() == &Token::Dot {
                self.advance();
                fields.push(self.parse_identifier()?);
            } else if self.current() == &Token::Colon {
                self.advance();
                method = Some(self.parse_identifier()?);
                break;
            } else {
                break;
            }
        }

        Ok(FunctionName { base, fields, method })
    }

    fn parse_function_body(&mut self, name: FunctionName, local: bool) -> Result<Stmt, String> {
        self.expect(Token::LParen)?;

        let mut params = Vec::new();
        let mut vararg = false;

        if self.current() != &Token::RParen {
            if self.current() == &Token::Vararg {
                self.advance();
                vararg = true;
            } else {
                params.push(self.parse_identifier()?);

                while self.current() == &Token::Comma {
                    self.advance();
                    if self.current() == &Token::Vararg {
                        self.advance();
                        vararg = true;
                        break;
                    }
                    params.push(self.parse_identifier()?);
                }
            }
        }

        self.expect(Token::RParen)?;
        let body = self.parse_block(&[Token::End])?;
        self.expect(Token::End)?;

        Ok(Stmt::Function { name, params, vararg, body, local })
    }

    fn parse_return(&mut self) -> Result<Stmt, String> {
        self.expect(Token::Return)?;

        let mut values = Vec::new();

        // Check if there's a return value
        if !matches!(
            self.current(),
            Token::End | Token::Else | Token::Elseif | Token::Until | Token::Eof
        ) {
            values.push(self.parse_expression()?);

            while self.current() == &Token::Comma {
                self.advance();
                values.push(self.parse_expression()?);
            }
        }

        Ok(Stmt::Return(values))
    }

    fn parse_do(&mut self) -> Result<Stmt, String> {
        self.expect(Token::Do)?;
        let body = self.parse_block(&[Token::End])?;
        self.expect(Token::End)?;
        Ok(Stmt::Do(body))
    }

    fn parse_goto(&mut self) -> Result<Stmt, String> {
        self.expect(Token::Goto)?;
        let name = self.parse_identifier()?;
        Ok(Stmt::Goto(name))
    }

    fn parse_label(&mut self) -> Result<Stmt, String> {
        self.expect(Token::DoubleColon)?;
        let name = self.parse_identifier()?;
        self.expect(Token::DoubleColon)?;
        Ok(Stmt::Label(name))
    }

    fn parse_assignment_or_call(&mut self) -> Result<Stmt, String> {
        let expr = self.parse_prefix_expression()?;
        let expr = self.parse_expression_rest(expr)?;

        // Check for assignment
        if self.current() == &Token::Assign {
            self.advance();
            let value = self.parse_expression()?;

            let target = self.expr_to_assign_target(expr)?;
            return Ok(Stmt::Assign {
                targets: vec![target],
                values: vec![value],
                local: false,
            });
        }

        // Multiple assignment: a, b = c, d
        if self.current() == &Token::Comma {
            let mut targets = vec![self.expr_to_assign_target(expr)?];

            while self.current() == &Token::Comma {
                self.advance();
                let next_expr = self.parse_prefix_expression()?;
                targets.push(self.expr_to_assign_target(next_expr)?);
            }

            self.expect(Token::Assign)?;

            let mut values = vec![self.parse_expression()?];
            while self.current() == &Token::Comma {
                self.advance();
                values.push(self.parse_expression()?);
            }

            return Ok(Stmt::Assign {
                targets,
                values,
                local: false,
            });
        }

        // Expression statement (function call)
        let full_expr = self.parse_expression_rest(expr)?;
        Ok(Stmt::Expression(full_expr))
    }

    fn expr_to_assign_target(&self, expr: Expr) -> Result<AssignTarget, String> {
        match expr {
            Expr::Variable(name) => Ok(AssignTarget::Name(name)),
            Expr::Index(table, key) => Ok(AssignTarget::Index(*table, *key)),
            _ => Err("Invalid assignment target".to_string()),
        }
    }

    fn parse_block(&mut self, terminators: &[Token]) -> Result<Vec<Stmt>, String> {
        let mut statements = Vec::new();

        while !terminators.contains(self.current()) && self.current() != &Token::Eof {
            statements.push(self.parse_statement()?);
        }

        Ok(statements)
    }

    fn parse_identifier(&mut self) -> Result<String, String> {
        match self.current().clone() {
            Token::Identifier(name) => {
                self.advance();
                Ok(name)
            }
            _ => Err(format!("Expected identifier, found {:?}", self.current())),
        }
    }

    fn parse_expression(&mut self) -> Result<Expr, String> {
        self.parse_or_expression()
    }

    fn parse_or_expression(&mut self) -> Result<Expr, String> {
        let mut left = self.parse_and_expression()?;

        while self.current() == &Token::Or {
            self.advance();
            let right = self.parse_and_expression()?;
            left = Expr::BinaryOp {
                left: Box::new(left),
                op: BinaryOperator::Or,
                right: Box::new(right),
            };
        }

        Ok(left)
    }

    fn parse_and_expression(&mut self) -> Result<Expr, String> {
        let mut left = self.parse_comparison_expression()?;

        while self.current() == &Token::And {
            self.advance();
            let right = self.parse_comparison_expression()?;
            left = Expr::BinaryOp {
                left: Box::new(left),
                op: BinaryOperator::And,
                right: Box::new(right),
            };
        }

        Ok(left)
    }

    fn parse_comparison_expression(&mut self) -> Result<Expr, String> {
        let mut left = self.parse_bitor_expression()?;

        loop {
            let op = match self.current() {
                Token::Eq => BinaryOperator::Eq,
                Token::NotEq => BinaryOperator::NotEq,
                Token::Lt => BinaryOperator::Lt,
                Token::Gt => BinaryOperator::Gt,
                Token::Le => BinaryOperator::Le,
                Token::Ge => BinaryOperator::Ge,
                _ => break,
            };
            self.advance();
            let right = self.parse_bitor_expression()?;
            left = Expr::BinaryOp {
                left: Box::new(left),
                op,
                right: Box::new(right),
            };
        }

        Ok(left)
    }

    fn parse_bitor_expression(&mut self) -> Result<Expr, String> {
        let mut left = self.parse_bitxor_expression()?;

        while self.current() == &Token::Pipe {
            self.advance();
            let right = self.parse_bitxor_expression()?;
            left = Expr::BinaryOp {
                left: Box::new(left),
                op: BinaryOperator::BOr,
                right: Box::new(right),
            };
        }

        Ok(left)
    }

    fn parse_bitxor_expression(&mut self) -> Result<Expr, String> {
        let mut left = self.parse_bitand_expression()?;

        while self.current() == &Token::Tilde {
            self.advance();
            let right = self.parse_bitand_expression()?;
            left = Expr::BinaryOp {
                left: Box::new(left),
                op: BinaryOperator::BXor,
                right: Box::new(right),
            };
        }

        Ok(left)
    }

    fn parse_bitand_expression(&mut self) -> Result<Expr, String> {
        let mut left = self.parse_shift_expression()?;

        while self.current() == &Token::Ampersand {
            self.advance();
            let right = self.parse_shift_expression()?;
            left = Expr::BinaryOp {
                left: Box::new(left),
                op: BinaryOperator::BAnd,
                right: Box::new(right),
            };
        }

        Ok(left)
    }

    fn parse_shift_expression(&mut self) -> Result<Expr, String> {
        let mut left = self.parse_concat_expression()?;

        loop {
            let op = match self.current() {
                Token::Shl => BinaryOperator::Shl,
                Token::Shr => BinaryOperator::Shr,
                _ => break,
            };
            self.advance();
            let right = self.parse_concat_expression()?;
            left = Expr::BinaryOp {
                left: Box::new(left),
                op,
                right: Box::new(right),
            };
        }

        Ok(left)
    }

    fn parse_concat_expression(&mut self) -> Result<Expr, String> {
        let mut left = self.parse_additive_expression()?;

        // Concat is right-associative
        if self.current() == &Token::Concat {
            self.advance();
            let right = self.parse_concat_expression()?;
            left = Expr::BinaryOp {
                left: Box::new(left),
                op: BinaryOperator::Concat,
                right: Box::new(right),
            };
        }

        Ok(left)
    }

    fn parse_additive_expression(&mut self) -> Result<Expr, String> {
        let mut left = self.parse_multiplicative_expression()?;

        loop {
            let op = match self.current() {
                Token::Plus => BinaryOperator::Add,
                Token::Minus => BinaryOperator::Sub,
                _ => break,
            };
            self.advance();
            let right = self.parse_multiplicative_expression()?;
            left = Expr::BinaryOp {
                left: Box::new(left),
                op,
                right: Box::new(right),
            };
        }

        Ok(left)
    }

    fn parse_multiplicative_expression(&mut self) -> Result<Expr, String> {
        let mut left = self.parse_unary_expression()?;

        loop {
            let op = match self.current() {
                Token::Star => BinaryOperator::Mul,
                Token::Slash => BinaryOperator::Div,
                Token::DoubleSlash => BinaryOperator::IDiv,
                Token::Percent => BinaryOperator::Mod,
                _ => break,
            };
            self.advance();
            let right = self.parse_unary_expression()?;
            left = Expr::BinaryOp {
                left: Box::new(left),
                op,
                right: Box::new(right),
            };
        }

        Ok(left)
    }

    fn parse_unary_expression(&mut self) -> Result<Expr, String> {
        match self.current() {
            Token::Minus => {
                self.advance();
                let operand = self.parse_unary_expression()?;
                Ok(Expr::UnaryOp {
                    op: UnaryOperator::Neg,
                    operand: Box::new(operand),
                })
            }
            Token::Not => {
                self.advance();
                let operand = self.parse_unary_expression()?;
                Ok(Expr::UnaryOp {
                    op: UnaryOperator::Not,
                    operand: Box::new(operand),
                })
            }
            Token::Hash => {
                self.advance();
                let operand = self.parse_unary_expression()?;
                Ok(Expr::UnaryOp {
                    op: UnaryOperator::Len,
                    operand: Box::new(operand),
                })
            }
            Token::Tilde => {
                self.advance();
                let operand = self.parse_unary_expression()?;
                Ok(Expr::UnaryOp {
                    op: UnaryOperator::BNot,
                    operand: Box::new(operand),
                })
            }
            _ => self.parse_power_expression(),
        }
    }

    fn parse_power_expression(&mut self) -> Result<Expr, String> {
        let left = self.parse_prefix_expression()?;

        if self.current() == &Token::Caret {
            self.advance();
            let right = self.parse_unary_expression()?;
            Ok(Expr::BinaryOp {
                left: Box::new(left),
                op: BinaryOperator::Pow,
                right: Box::new(right),
            })
        } else {
            self.parse_expression_rest(left)
        }
    }

    fn parse_prefix_expression(&mut self) -> Result<Expr, String> {
        match self.current().clone() {
            Token::Nil => {
                self.advance();
                Ok(Expr::LiteralNil)
            }
            Token::True => {
                self.advance();
                Ok(Expr::LiteralBool(true))
            }
            Token::False => {
                self.advance();
                Ok(Expr::LiteralBool(false))
            }
            Token::Number(n) => {
                self.advance();
                Ok(Expr::LiteralNumber(n))
            }
            Token::String(s) => {
                self.advance();
                Ok(Expr::LiteralString(s))
            }
            Token::Identifier(name) => {
                self.advance();
                Ok(Expr::Variable(name))
            }
            Token::Vararg => {
                self.advance();
                Ok(Expr::Vararg)
            }
            Token::LParen => {
                self.advance();
                let expr = self.parse_expression()?;
                self.expect(Token::RParen)?;
                Ok(expr)
            }
            Token::LBrace => self.parse_table_constructor(),
            Token::Function => self.parse_anonymous_function(),
            _ => Err(format!("Unexpected token: {:?}", self.current())),
        }
    }

    fn parse_anonymous_function(&mut self) -> Result<Expr, String> {
        self.expect(Token::Function)?;
        self.expect(Token::LParen)?;

        let mut params = Vec::new();
        let mut vararg = false;

        if self.current() != &Token::RParen {
            if self.current() == &Token::Vararg {
                self.advance();
                vararg = true;
            } else {
                params.push(self.parse_identifier()?);

                while self.current() == &Token::Comma {
                    self.advance();
                    if self.current() == &Token::Vararg {
                        self.advance();
                        vararg = true;
                        break;
                    }
                    params.push(self.parse_identifier()?);
                }
            }
        }

        self.expect(Token::RParen)?;
        let body = self.parse_block(&[Token::End])?;
        self.expect(Token::End)?;

        Ok(Expr::Function { params, vararg, body })
    }

    fn parse_expression_rest(&mut self, mut expr: Expr) -> Result<Expr, String> {
        loop {
            match self.current() {
                Token::Dot => {
                    self.advance();
                    let field = self.parse_identifier()?;
                    expr = Expr::Index(
                        Box::new(expr),
                        Box::new(Expr::LiteralString(field)),
                    );
                }
                Token::LBracket => {
                    self.advance();
                    let index = self.parse_expression()?;
                    self.expect(Token::RBracket)?;
                    expr = Expr::Index(Box::new(expr), Box::new(index));
                }
                Token::LParen => {
                    self.advance();
                    let mut args = Vec::new();

                    if self.current() != &Token::RParen {
                        args.push(self.parse_expression()?);

                        while self.current() == &Token::Comma {
                            self.advance();
                            args.push(self.parse_expression()?);
                        }
                    }

                    self.expect(Token::RParen)?;
                    expr = Expr::Call {
                        func: Box::new(expr),
                        args,
                    };
                }
                Token::Colon => {
                    // Method call: obj:method(args)
                    self.advance();
                    let method = self.parse_identifier()?;
                    self.expect(Token::LParen)?;

                    let mut args = vec![expr.clone()]; // self as first arg

                    if self.current() != &Token::RParen {
                        args.push(self.parse_expression()?);

                        while self.current() == &Token::Comma {
                            self.advance();
                            args.push(self.parse_expression()?);
                        }
                    }

                    self.expect(Token::RParen)?;

                    let func = Expr::Index(
                        Box::new(expr),
                        Box::new(Expr::LiteralString(method)),
                    );

                    expr = Expr::Call {
                        func: Box::new(func),
                        args,
                    };
                }
                Token::String(s) => {
                    // Function call with string argument: print "hello"
                    let s = s.clone();
                    self.advance();
                    expr = Expr::Call {
                        func: Box::new(expr),
                        args: vec![Expr::LiteralString(s)],
                    };
                }
                Token::LBrace => {
                    // Function call with table argument: print {}
                    let table = self.parse_table_constructor()?;
                    expr = Expr::Call {
                        func: Box::new(expr),
                        args: vec![table],
                    };
                }
                _ => break,
            }
        }

        Ok(expr)
    }

    fn parse_table_constructor(&mut self) -> Result<Expr, String> {
        self.expect(Token::LBrace)?;

        let mut entries = Vec::new();

        while self.current() != &Token::RBrace {
            // [key] = value
            if self.current() == &Token::LBracket {
                self.advance();
                let key = self.parse_expression()?;
                self.expect(Token::RBracket)?;
                self.expect(Token::Assign)?;
                let value = self.parse_expression()?;
                entries.push(TableEntry::KeyValue(key, value));
            }
            // key = value (identifier key)
            else if let Token::Identifier(name) = self.current().clone() {
                if self.peek(1) == &Token::Assign {
                    self.advance();
                    self.advance();
                    let value = self.parse_expression()?;
                    entries.push(TableEntry::KeyValue(Expr::LiteralString(name), value));
                } else {
                    // array element
                    let value = self.parse_expression()?;
                    entries.push(TableEntry::Array(value));
                }
            } else {
                // array element
                let value = self.parse_expression()?;
                entries.push(TableEntry::Array(value));
            }

            // Optional separator
            if self.current() == &Token::Comma || self.current() == &Token::Semicolon {
                self.advance();
            } else {
                break;
            }
        }

        self.expect(Token::RBrace)?;

        Ok(Expr::Table(entries))
    }
}
