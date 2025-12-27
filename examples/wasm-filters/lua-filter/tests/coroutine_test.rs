//! Unit tests for coroutine functionality

use lua_filter::lua::interpreter::{Interpreter, SharedState};
use lua_filter::lua::{lexer, parser};
use std::cell::RefCell;
use std::rc::Rc;

fn create_interpreter() -> Interpreter {
    let state = Rc::new(RefCell::new(SharedState::default()));
    Interpreter::with_state(state)
}

// ============================================================================
// Basic coroutine functionality
// ============================================================================

#[test]
fn test_coroutine_create() {
    let mut interpreter = create_interpreter();
    let tokens = lexer::tokenize(
        "function f() return 42 end; local co = coroutine.create(f); return type(co)"
    ).unwrap();
    let program = parser::parse(&tokens).unwrap();
    let result = interpreter.execute(&program).unwrap();
    assert_eq!(result.to_lua_string(), "thread");
}

#[test]
fn test_coroutine_create_status() {
    let mut interpreter = create_interpreter();
    let tokens = lexer::tokenize(
        "function f() return 42 end; \
         local co = coroutine.create(f); \
         return coroutine.status(co)"
    ).unwrap();
    let program = parser::parse(&tokens).unwrap();
    let result = interpreter.execute(&program).unwrap();
    assert_eq!(result.to_lua_string(), "suspended");
}

#[test]
fn test_coroutine_resume_basic() {
    let mut interpreter = create_interpreter();
    let tokens = lexer::tokenize(
        "function f() return 42 end; \
         local co = coroutine.create(f); \
         local ok, result = coroutine.resume(co); \
         return ok and result"
    ).unwrap();
    let program = parser::parse(&tokens).unwrap();
    let result = interpreter.execute(&program).unwrap();
    assert_eq!(result.to_number(), Some(42.0));
}

#[test]
fn test_coroutine_resume_with_args() {
    let mut interpreter = create_interpreter();
    let tokens = lexer::tokenize(
        "function f(x, y) return x + y end; \
         local co = coroutine.create(f); \
         local ok, result = coroutine.resume(co, 10, 20); \
         return ok and result"
    ).unwrap();
    let program = parser::parse(&tokens).unwrap();
    let result = interpreter.execute(&program).unwrap();
    assert_eq!(result.to_number(), Some(30.0));
}

#[test]
fn test_coroutine_resume_multiple_returns() {
    let mut interpreter = create_interpreter();
    let tokens = lexer::tokenize(
        "function f() return 1, 2, 3 end; \
         local co = coroutine.create(f); \
         local ok, a, b, c = coroutine.resume(co); \
         return ok and a == 1 and b == 2 and c == 3"
    ).unwrap();
    let program = parser::parse(&tokens).unwrap();
    let result = interpreter.execute(&program).unwrap();
    assert_eq!(result.is_truthy(), true);
}

#[test]
fn test_coroutine_yield_basic() {
    let mut interpreter = create_interpreter();
    let tokens = lexer::tokenize(
        "function f() coroutine.yield(42); return 100 end; \
         local co = coroutine.create(f); \
         local ok1, val1 = coroutine.resume(co); \
         local ok2, val2 = coroutine.resume(co); \
         return ok1 and val1 == 42 and ok2 and val2 == 100"
    ).unwrap();
    let program = parser::parse(&tokens).unwrap();
    let result = interpreter.execute(&program).unwrap();
    assert_eq!(result.is_truthy(), true);
}

#[test]
fn test_coroutine_yield_multiple_values() {
    let mut interpreter = create_interpreter();
    let tokens = lexer::tokenize(
        "function f() coroutine.yield(1, 2, 3); return 4 end; \
         local co = coroutine.create(f); \
         local ok1, a, b, c = coroutine.resume(co); \
         local ok2, d = coroutine.resume(co); \
         return ok1 and a == 1 and b == 2 and c == 3 and ok2 and d == 4"
    ).unwrap();
    let program = parser::parse(&tokens).unwrap();
    let result = interpreter.execute(&program).unwrap();
    assert_eq!(result.is_truthy(), true);
}

#[test]
fn test_coroutine_yield_with_args() {
    let mut interpreter = create_interpreter();
    let tokens = lexer::tokenize(
        "function f(x) \
             local y = coroutine.yield(x * 2); \
             return y * 3 \
         end; \
         local co = coroutine.create(f); \
         local ok1, val1 = coroutine.resume(co, 10); \
         local ok2, val2 = coroutine.resume(co, 5); \
         return ok1 and val1 == 20 and ok2 and val2 == 15"
    ).unwrap();
    let program = parser::parse(&tokens).unwrap();
    let result = interpreter.execute(&program).unwrap();
    assert_eq!(result.is_truthy(), true);
}

#[test]
fn test_coroutine_status() {
    let mut interpreter = create_interpreter();
    let tokens = lexer::tokenize(
        "function f() return 42 end; \
         local co = coroutine.create(f); \
         local status1 = coroutine.status(co); \
         coroutine.resume(co); \
         local status2 = coroutine.status(co); \
         return status1 == 'suspended' and status2 == 'dead'"
    ).unwrap();
    let program = parser::parse(&tokens).unwrap();
    let result = interpreter.execute(&program).unwrap();
    assert_eq!(result.is_truthy(), true);
}

#[test]
fn test_coroutine_wrap() {
    let mut interpreter = create_interpreter();
    let tokens = lexer::tokenize(
        "function f() coroutine.yield(42); return 100 end; \
         local wrapped = coroutine.wrap(f); \
         local val1 = wrapped(); \
         local val2 = wrapped(); \
         return val1 == 42 and val2 == 100"
    ).unwrap();
    let program = parser::parse(&tokens).unwrap();
    let result = interpreter.execute(&program).unwrap();
    assert_eq!(result.is_truthy(), true);
}

#[test]
fn test_coroutine_wrap_with_args() {
    let mut interpreter = create_interpreter();
    let tokens = lexer::tokenize(
        "function f(x, y) coroutine.yield(x + y); return x * y end; \
         local wrapped = coroutine.wrap(f); \
         local val1 = wrapped(10, 20); \
         local val2 = wrapped(); \
         return val1 == 30 and val2 == 200"
    ).unwrap();
    let program = parser::parse(&tokens).unwrap();
    let result = interpreter.execute(&program).unwrap();
    assert_eq!(result.is_truthy(), true);
}

// ============================================================================
// Complex scenarios
// ============================================================================

#[test]
fn test_nested_coroutines() {
    let mut interpreter = create_interpreter();
    let tokens = lexer::tokenize(
        "function inner() coroutine.yield(10); return 20 end; \
         function outer() \
             local co = coroutine.create(inner); \
             local ok, val = coroutine.resume(co); \
             return val \
         end; \
         local co = coroutine.create(outer); \
         local ok, val = coroutine.resume(co); \
         return val == 10"
    ).unwrap();
    let program = parser::parse(&tokens).unwrap();
    let result = interpreter.execute(&program).unwrap();
    assert_eq!(result.is_truthy(), true);
}

#[test]
fn test_yield_in_while_loop() {
    let mut interpreter = create_interpreter();
    let tokens = lexer::tokenize(
        "function f() local i = 0; while i < 3 do i = i + 1; coroutine.yield(i); end; return i end; \
         local co = coroutine.create(f); \
         local ok1, val1 = coroutine.resume(co); \
         local ok2, val2 = coroutine.resume(co); \
         local ok3, val3 = coroutine.resume(co); \
         local ok4, val4 = coroutine.resume(co); \
         return val1 == 1 and val2 == 2 and val3 == 3 and val4 == 3"
    ).unwrap();
    let program = parser::parse(&tokens).unwrap();
    let result = interpreter.execute(&program).unwrap();
    assert_eq!(result.is_truthy(), true);
}

#[test]
fn test_yield_in_for_loop() {
    let mut interpreter = create_interpreter();
    let tokens = lexer::tokenize(
        "function f() local sum = 0; for i = 1, 5 do sum = sum + i; coroutine.yield(sum); end; return sum end; \
         local co = coroutine.create(f); \
         local ok1, val1 = coroutine.resume(co); \
         local ok2, val2 = coroutine.resume(co); \
         local ok3, val3 = coroutine.resume(co); \
         return val1 == 1 and val2 == 3 and val3 == 6"
    ).unwrap();
    let program = parser::parse(&tokens).unwrap();
    let result = interpreter.execute(&program).unwrap();
    assert_eq!(result.is_truthy(), true);
}

#[test]
fn test_multiple_yields() {
    let mut interpreter = create_interpreter();
    let tokens = lexer::tokenize(
        "function f() \
             coroutine.yield(1); \
             coroutine.yield(2); \
             coroutine.yield(3); \
             return 4 \
         end; \
         local co = coroutine.create(f); \
         local ok1, val1 = coroutine.resume(co); \
         local ok2, val2 = coroutine.resume(co); \
         local ok3, val3 = coroutine.resume(co); \
         local ok4, val4 = coroutine.resume(co); \
         return val1 == 1 and val2 == 2 and val3 == 3 and val4 == 4"
    ).unwrap();
    let program = parser::parse(&tokens).unwrap();
    let result = interpreter.execute(&program).unwrap();
    assert_eq!(result.is_truthy(), true);
}

#[test]
fn test_coroutine_scope() {
    let mut interpreter = create_interpreter();
    let tokens = lexer::tokenize(
        "local x = 10; \
         function f() \
             local y = 20; \
             coroutine.yield(x + y); \
             return x + y \
         end; \
         local co = coroutine.create(f); \
         local ok1, val1 = coroutine.resume(co); \
         local ok2, val2 = coroutine.resume(co); \
         return val1 == 30 and val2 == 30"
    ).unwrap();
    let program = parser::parse(&tokens).unwrap();
    let result = interpreter.execute(&program).unwrap();
    assert_eq!(result.is_truthy(), true);
}

#[test]
fn test_coroutine_upvalue() {
    let mut interpreter = create_interpreter();
    let tokens = lexer::tokenize(
        "local x = 10; \
         function outer() \
             local y = 20; \
             function inner() \
                 coroutine.yield(x + y); \
                 return x + y \
             end; \
             return inner \
         end; \
         local f = outer(); \
         local co = coroutine.create(f); \
         local ok1, val1 = coroutine.resume(co); \
         local ok2, val2 = coroutine.resume(co); \
         return val1 == 30 and val2 == 30"
    ).unwrap();
    let program = parser::parse(&tokens).unwrap();
    let result = interpreter.execute(&program).unwrap();
    assert_eq!(result.is_truthy(), true);
}

// ============================================================================
// Error handling
// ============================================================================

#[test]
fn test_coroutine_error() {
    let mut interpreter = create_interpreter();
    let tokens = lexer::tokenize(
        "function f() error('test error') end; \
         local co = coroutine.create(f); \
         local ok, err = coroutine.resume(co); \
         return not ok and type(err) == 'string'"
    ).unwrap();
    let program = parser::parse(&tokens).unwrap();
    let result = interpreter.execute(&program).unwrap();
    assert_eq!(result.is_truthy(), true);
}

#[test]
fn test_resume_dead_coroutine() {
    let mut interpreter = create_interpreter();
    let tokens = lexer::tokenize(
        "function f() return 42 end; \
         local co = coroutine.create(f); \
         coroutine.resume(co); \
         local ok, err = coroutine.resume(co); \
         return not ok and type(err) == 'string'"
    ).unwrap();
    let program = parser::parse(&tokens).unwrap();
    let result = interpreter.execute(&program).unwrap();
    assert_eq!(result.is_truthy(), true);
}

#[test]
fn test_yield_outside_coroutine() {
    let mut interpreter = create_interpreter();
    let tokens = lexer::tokenize(
        "local ok, err = pcall(function() coroutine.yield(42) end); \
         return not ok and type(err) == 'string'"
    ).unwrap();
    let program = parser::parse(&tokens).unwrap();
    let result = interpreter.execute(&program).unwrap();
    assert_eq!(result.is_truthy(), true);
}

