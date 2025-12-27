//! Lua Pattern Matching Engine
//!
//! Implements Lua's pattern matching syntax, which is different from regular expressions.
//! Supports: . %a %d %s %w %p %c %l %u %x [set] [^set] * + - ? ^ $ () %bxy %1-%9

/// A compiled Lua pattern
#[derive(Debug, Clone)]
pub struct Pattern {
    items: Vec<PatternItem>,
}

#[derive(Debug, Clone)]
enum PatternItem {
    /// Literal character
    Literal(char),
    /// Any character (.)
    Any,
    /// Character class (%a, %d, etc.)
    Class(CharClass),
    /// Complement of class (%A, %D, etc.)
    NotClass(CharClass),
    /// Character set [abc] or [^abc]
    Set(Vec<SetItem>, bool), // bool = negated
    /// Quantifier
    Quantified(Box<PatternItem>, Quantifier),
    /// Capture group
    Capture(Vec<PatternItem>),
    /// Balanced match %bxy
    Balanced(char, char),
    /// Back reference %1-%9
    BackRef(usize),
    /// Start anchor ^
    StartAnchor,
    /// End anchor $
    EndAnchor,
    /// Frontier pattern %f[set]
    Frontier(Vec<SetItem>),
}

#[derive(Debug, Clone, Copy)]
enum CharClass {
    Alpha,    // %a - letters
    Digit,    // %d - digits
    Space,    // %s - whitespace
    Alnum,    // %w - alphanumeric
    Punct,    // %p - punctuation
    Ctrl,     // %c - control characters
    Lower,    // %l - lowercase
    Upper,    // %u - uppercase
    Hex,      // %x - hex digits
    Zero,     // %z - null character (Lua 5.1)
}

#[derive(Debug, Clone)]
enum SetItem {
    Char(char),
    Range(char, char),
    Class(CharClass),
}

#[derive(Debug, Clone, Copy)]
enum Quantifier {
    ZeroOrMore,  // *
    OneOrMore,   // +
    ZeroOrMoreLazy, // -
    ZeroOrOne,   // ?
}

impl CharClass {
    fn matches(&self, c: char) -> bool {
        match self {
            CharClass::Alpha => c.is_alphabetic(),
            CharClass::Digit => c.is_ascii_digit(),
            CharClass::Space => c.is_whitespace(),
            CharClass::Alnum => c.is_alphanumeric(),
            CharClass::Punct => c.is_ascii_punctuation(),
            CharClass::Ctrl => c.is_control(),
            CharClass::Lower => c.is_lowercase(),
            CharClass::Upper => c.is_uppercase(),
            CharClass::Hex => c.is_ascii_hexdigit(),
            CharClass::Zero => c == '\0',
        }
    }
}

impl Pattern {
    /// Compile a Lua pattern string
    pub fn compile(pattern: &str) -> Result<Self, String> {
        let mut items = Vec::new();
        let mut chars = pattern.chars().peekable();
        
        // Check for start anchor
        if chars.peek() == Some(&'^') {
            chars.next();
            items.push(PatternItem::StartAnchor);
        }
        
        while let Some(c) = chars.next() {
            // Check for end anchor
            if c == '$' && chars.peek().is_none() {
                items.push(PatternItem::EndAnchor);
                continue;
            }
            
            let item = match c {
                '.' => PatternItem::Any,
                '%' => {
                    let next = chars.next().ok_or("Pattern ends with %")?;
                    match next {
                        'a' => PatternItem::Class(CharClass::Alpha),
                        'd' => PatternItem::Class(CharClass::Digit),
                        's' => PatternItem::Class(CharClass::Space),
                        'w' => PatternItem::Class(CharClass::Alnum),
                        'p' => PatternItem::Class(CharClass::Punct),
                        'c' => PatternItem::Class(CharClass::Ctrl),
                        'l' => PatternItem::Class(CharClass::Lower),
                        'u' => PatternItem::Class(CharClass::Upper),
                        'x' => PatternItem::Class(CharClass::Hex),
                        'z' => PatternItem::Class(CharClass::Zero),
                        'A' => PatternItem::NotClass(CharClass::Alpha),
                        'D' => PatternItem::NotClass(CharClass::Digit),
                        'S' => PatternItem::NotClass(CharClass::Space),
                        'W' => PatternItem::NotClass(CharClass::Alnum),
                        'P' => PatternItem::NotClass(CharClass::Punct),
                        'C' => PatternItem::NotClass(CharClass::Ctrl),
                        'L' => PatternItem::NotClass(CharClass::Lower),
                        'U' => PatternItem::NotClass(CharClass::Upper),
                        'X' => PatternItem::NotClass(CharClass::Hex),
                        'Z' => PatternItem::NotClass(CharClass::Zero),
                        'b' => {
                            let open = chars.next().ok_or("%b needs two chars")?;
                            let close = chars.next().ok_or("%b needs two chars")?;
                            PatternItem::Balanced(open, close)
                        }
                        'f' => {
                            // Frontier pattern: %f[set]
                            if chars.peek() == Some(&'[') {
                                chars.next(); // consume '['
                                let (set, _) = Self::parse_set(&mut chars)?;
                                PatternItem::Frontier(set)
                            } else {
                                return Err("%f must be followed by [set]".to_string());
                            }
                        }
                        '1'..='9' => {
                            let idx = next.to_digit(10).unwrap() as usize;
                            PatternItem::BackRef(idx)
                        }
                        // Escaped special characters
                        c @ ('.' | '%' | '[' | ']' | '(' | ')' | '*' | '+' | '-' | '?' | '^' | '$') => {
                            PatternItem::Literal(c)
                        }
                        _ => PatternItem::Literal(next),
                    }
                }
                '[' => {
                    let (set, negated) = Self::parse_set(&mut chars)?;
                    PatternItem::Set(set, negated)
                }
                '(' => {
                    // Simple capture - parse until matching )
                    let mut depth = 1;
                    let mut capture_str = String::new();
                    while depth > 0 {
                        let c = chars.next().ok_or("Unmatched (")?;
                        if c == '(' {
                            depth += 1;
                        } else if c == ')' {
                            depth -= 1;
                            if depth > 0 {
                                capture_str.push(c);
                            }
                        } else {
                            capture_str.push(c);
                        }
                    }
                    let inner = Pattern::compile(&capture_str)?;
                    PatternItem::Capture(inner.items)
                }
                _ => PatternItem::Literal(c),
            };
            
            // Check for quantifier
            let item = match chars.peek() {
                Some('*') => {
                    chars.next();
                    PatternItem::Quantified(Box::new(item), Quantifier::ZeroOrMore)
                }
                Some('+') => {
                    chars.next();
                    PatternItem::Quantified(Box::new(item), Quantifier::OneOrMore)
                }
                Some('-') => {
                    chars.next();
                    PatternItem::Quantified(Box::new(item), Quantifier::ZeroOrMoreLazy)
                }
                Some('?') => {
                    chars.next();
                    PatternItem::Quantified(Box::new(item), Quantifier::ZeroOrOne)
                }
                _ => item,
            };
            
            items.push(item);
        }
        
        Ok(Pattern { items })
    }
    
    fn parse_set(chars: &mut std::iter::Peekable<std::str::Chars>) -> Result<(Vec<SetItem>, bool), String> {
        let mut items = Vec::new();
        let negated = chars.peek() == Some(&'^');
        if negated {
            chars.next();
        }
        
        let mut prev_char: Option<char> = None;
        
        while let Some(&c) = chars.peek() {
            if c == ']' && !items.is_empty() {
                chars.next();
                break;
            }
            chars.next();
            
            match c {
                '%' => {
                    let next = chars.next().ok_or("Pattern ends in [")?;
                    let class = match next {
                        'a' => CharClass::Alpha,
                        'd' => CharClass::Digit,
                        's' => CharClass::Space,
                        'w' => CharClass::Alnum,
                        'p' => CharClass::Punct,
                        'c' => CharClass::Ctrl,
                        'l' => CharClass::Lower,
                        'u' => CharClass::Upper,
                        'x' => CharClass::Hex,
                        'z' => CharClass::Zero,
                        _ => {
                            items.push(SetItem::Char(next));
                            prev_char = Some(next);
                            continue;
                        }
                    };
                    items.push(SetItem::Class(class));
                    prev_char = None;
                }
                '-' if prev_char.is_some() && chars.peek().is_some() && chars.peek() != Some(&']') => {
                    // Range
                    let end = chars.next().unwrap();
                    let start = prev_char.take().unwrap();
                    // Remove the previous char item
                    items.pop();
                    items.push(SetItem::Range(start, end));
                }
                _ => {
                    items.push(SetItem::Char(c));
                    prev_char = Some(c);
                }
            }
        }
        
        Ok((items, negated))
    }
}

/// Match result with captures
#[derive(Debug, Clone)]
pub struct MatchResult {
    pub matched: String,
    pub start: usize,
    pub end: usize,
    pub captures: Vec<String>,
}

/// Match a pattern against a string
pub fn match_pattern(s: &str, pattern: &str) -> Result<Option<MatchResult>, String> {
    let pat = Pattern::compile(pattern)?;
    let chars: Vec<char> = s.chars().collect();
    
    let has_start_anchor = matches!(pat.items.first(), Some(PatternItem::StartAnchor));
    
    if has_start_anchor {
        // Only try matching at start
        if let Some(result) = try_match(&chars, 0, &pat.items, &[]) {
            return Ok(Some(result));
        }
    } else {
        // Try matching at each position
        for start in 0..=chars.len() {
            if let Some(result) = try_match(&chars, start, &pat.items, &[]) {
                return Ok(Some(result));
            }
        }
    }
    
    Ok(None)
}

/// Try to match pattern items at a given position
fn try_match(
    chars: &[char],
    pos: usize,
    items: &[PatternItem],
    captures: &[String],
) -> Option<MatchResult> {
    let mut pos = pos;
    let start_pos = pos;
    let mut captures: Vec<String> = captures.to_vec();
    let mut item_idx = 0;
    
    while item_idx < items.len() {
        let item = &items[item_idx];
        
        match item {
            PatternItem::StartAnchor => {
                if pos != 0 {
                    return None;
                }
            }
            PatternItem::EndAnchor => {
                if pos != chars.len() {
                    return None;
                }
            }
            PatternItem::Frontier(set_items) => {
                // Frontier pattern: matches if previous char is NOT in set and current char IS in set
                let prev_in_set = if pos == 0 {
                    false
                } else {
                    let prev_char = chars[pos - 1];
                    set_items.iter().any(|item| match item {
                        SetItem::Char(sc) => prev_char == *sc,
                        SetItem::Range(start, end) => prev_char >= *start && prev_char <= *end,
                        SetItem::Class(class) => class.matches(prev_char),
                    })
                };
                
                let curr_in_set = if pos < chars.len() {
                    let curr_char = chars[pos];
                    set_items.iter().any(|item| match item {
                        SetItem::Char(sc) => curr_char == *sc,
                        SetItem::Range(start, end) => curr_char >= *start && curr_char <= *end,
                        SetItem::Class(class) => class.matches(curr_char),
                    })
                } else {
                    false
                };
                
                // Match if prev NOT in set AND curr IS in set
                if !prev_in_set && curr_in_set {
                    // Frontier matches at this position, but doesn't consume any characters
                    // Continue to next pattern item
                } else {
                    return None;
                }
            }
            PatternItem::Literal(c) => {
                if pos >= chars.len() || chars[pos] != *c {
                    return None;
                }
                pos += 1;
            }
            PatternItem::Any => {
                if pos >= chars.len() {
                    return None;
                }
                pos += 1;
            }
            PatternItem::Class(class) => {
                if pos >= chars.len() || !class.matches(chars[pos]) {
                    return None;
                }
                pos += 1;
            }
            PatternItem::NotClass(class) => {
                if pos >= chars.len() || class.matches(chars[pos]) {
                    return None;
                }
                pos += 1;
            }
            PatternItem::Set(set_items, negated) => {
                if pos >= chars.len() {
                    return None;
                }
                let c = chars[pos];
                let mut matches = false;
                for set_item in set_items {
                    match set_item {
                        SetItem::Char(sc) => {
                            if c == *sc {
                                matches = true;
                                break;
                            }
                        }
                        SetItem::Range(start, end) => {
                            if c >= *start && c <= *end {
                                matches = true;
                                break;
                            }
                        }
                        SetItem::Class(class) => {
                            if class.matches(c) {
                                matches = true;
                                break;
                            }
                        }
                    }
                }
                if *negated {
                    matches = !matches;
                }
                if !matches {
                    return None;
                }
                pos += 1;
            }
            PatternItem::Quantified(inner, quantifier) => {
                match quantifier {
                    Quantifier::ZeroOrMore => {
                        // Greedy: match as many as possible
                        let mut matched = 0;
                        while let Some(_) = match_single(chars, pos + matched, inner) {
                            matched += 1;
                        }
                        // Try rest of pattern with decreasing matches
                        while matched >= 0 {
                            if let Some(result) = try_match(
                                chars,
                                pos + matched as usize,
                                &items[item_idx + 1..],
                                &captures,
                            ) {
                                return Some(MatchResult {
                                    matched: chars[start_pos..result.end].iter().collect(),
                                    start: start_pos,
                                    end: result.end,
                                    captures: result.captures,
                                });
                            }
                            if matched == 0 {
                                break;
                            }
                            matched -= 1;
                        }
                        return None;
                    }
                    Quantifier::OneOrMore => {
                        // Must match at least once
                        if match_single(chars, pos, inner).is_none() {
                            return None;
                        }
                        let mut matched = 1;
                        while let Some(_) = match_single(chars, pos + matched, inner) {
                            matched += 1;
                        }
                        // Try rest of pattern with decreasing matches
                        while matched >= 1 {
                            if let Some(result) = try_match(
                                chars,
                                pos + matched,
                                &items[item_idx + 1..],
                                &captures,
                            ) {
                                return Some(MatchResult {
                                    matched: chars[start_pos..result.end].iter().collect(),
                                    start: start_pos,
                                    end: result.end,
                                    captures: result.captures,
                                });
                            }
                            matched -= 1;
                        }
                        return None;
                    }
                    Quantifier::ZeroOrMoreLazy => {
                        // Lazy: match as few as possible
                        let mut matched = 0;
                        loop {
                            if let Some(result) = try_match(
                                chars,
                                pos + matched,
                                &items[item_idx + 1..],
                                &captures,
                            ) {
                                return Some(MatchResult {
                                    matched: chars[start_pos..result.end].iter().collect(),
                                    start: start_pos,
                                    end: result.end,
                                    captures: result.captures,
                                });
                            }
                            if match_single(chars, pos + matched, inner).is_none() {
                                break;
                            }
                            matched += 1;
                        }
                        return None;
                    }
                    Quantifier::ZeroOrOne => {
                        // Try with one match
                        if match_single(chars, pos, inner).is_some() {
                            if let Some(result) = try_match(
                                chars,
                                pos + 1,
                                &items[item_idx + 1..],
                                &captures,
                            ) {
                                return Some(result);
                            }
                        }
                        // Try without match
                        if let Some(result) = try_match(
                            chars,
                            pos,
                            &items[item_idx + 1..],
                            &captures,
                        ) {
                            return Some(result);
                        }
                        return None;
                    }
                }
            }
            PatternItem::Capture(inner_items) => {
                let inner_pattern = Pattern {
                    items: inner_items.clone(),
                };
                if let Some(result) = try_match(chars, pos, &inner_pattern.items, &captures) {
                    captures.push(result.matched.clone());
                    pos = result.end;
                } else {
                    return None;
                }
            }
            PatternItem::Balanced(open, close) => {
                if pos >= chars.len() || chars[pos] != *open {
                    return None;
                }
                let mut depth = 1;
                let mut end = pos + 1;
                while depth > 0 && end < chars.len() {
                    if chars[end] == *open {
                        depth += 1;
                    } else if chars[end] == *close {
                        depth -= 1;
                    }
                    end += 1;
                }
                if depth != 0 {
                    return None;
                }
                pos = end;
            }
            PatternItem::BackRef(idx) => {
                if *idx > captures.len() {
                    return None;
                }
                let capture = &captures[*idx - 1];
                for c in capture.chars() {
                    if pos >= chars.len() || chars[pos] != c {
                        return None;
                    }
                    pos += 1;
                }
            }
        }
        
        item_idx += 1;
    }
    
    Some(MatchResult {
        matched: chars[start_pos..pos].iter().collect(),
        start: start_pos,
        end: pos,
        captures,
    })
}

fn match_single(chars: &[char], pos: usize, item: &PatternItem) -> Option<()> {
    if pos >= chars.len() {
        return None;
    }
    let c = chars[pos];
    
    match item {
        PatternItem::Literal(lc) => {
            if c == *lc {
                Some(())
            } else {
                None
            }
        }
        PatternItem::Any => Some(()),
        PatternItem::Class(class) => {
            if class.matches(c) {
                Some(())
            } else {
                None
            }
        }
        PatternItem::NotClass(class) => {
            if !class.matches(c) {
                Some(())
            } else {
                None
            }
        }
        PatternItem::Set(set_items, negated) => {
            let mut matches = false;
            for set_item in set_items {
                match set_item {
                    SetItem::Char(sc) => {
                        if c == *sc {
                            matches = true;
                            break;
                        }
                    }
                    SetItem::Range(start, end) => {
                        if c >= *start && c <= *end {
                            matches = true;
                            break;
                        }
                    }
                    SetItem::Class(class) => {
                        if class.matches(c) {
                            matches = true;
                            break;
                        }
                    }
                }
            }
            if *negated {
                matches = !matches;
            }
            if matches {
                Some(())
            } else {
                None
            }
        }
        _ => None,
    }
}

/// Find all matches (for gmatch)
pub fn match_all(s: &str, pattern: &str) -> Result<Vec<MatchResult>, String> {
    let pat = Pattern::compile(pattern)?;
    let chars: Vec<char> = s.chars().collect();
    let mut results = Vec::new();
    let mut pos = 0;
    
    while pos <= chars.len() {
        if let Some(result) = try_match(&chars, pos, &pat.items, &[]) {
            if result.end > pos {
                pos = result.end;
            } else {
                pos += 1;
            }
            results.push(result);
        } else {
            pos += 1;
        }
    }
    
    Ok(results)
}

/// Global substitution (for gsub)
pub fn gsub(s: &str, pattern: &str, replacement: &str, max_count: Option<usize>) -> Result<(String, usize), String> {
    let pat = Pattern::compile(pattern)?;
    let chars: Vec<char> = s.chars().collect();
    let mut result = String::new();
    let mut pos = 0;
    let mut count = 0;
    
    while pos <= chars.len() {
        if max_count.map(|m| count >= m).unwrap_or(false) {
            // Reached max substitutions
            result.extend(&chars[pos..]);
            break;
        }
        
        if let Some(m) = try_match(&chars, pos, &pat.items, &[]) {
            // Add text before match
            result.extend(&chars[pos..m.start]);
            
            // Process replacement
            let mut rep_chars = replacement.chars().peekable();
            while let Some(c) = rep_chars.next() {
                if c == '%' {
                    if let Some(&next) = rep_chars.peek() {
                        if next == '0' {
                            rep_chars.next();
                            result.push_str(&m.matched);
                        } else if let Some(d) = next.to_digit(10) {
                            rep_chars.next();
                            if let Some(cap) = m.captures.get(d as usize - 1) {
                                result.push_str(cap);
                            }
                        } else if next == '%' {
                            rep_chars.next();
                            result.push('%');
                        } else {
                            result.push(c);
                        }
                    } else {
                        result.push(c);
                    }
                } else {
                    result.push(c);
                }
            }
            
            pos = if m.end > pos { m.end } else { pos + 1 };
            count += 1;
        } else {
            if pos < chars.len() {
                result.push(chars[pos]);
            }
            pos += 1;
        }
    }
    
    Ok((result, count))
}

#[cfg(test)]
mod tests {
    use super::*;
    
    #[test]
    fn test_literal_match() {
        let result = match_pattern("hello", "ell").unwrap();
        assert!(result.is_some());
        assert_eq!(result.unwrap().matched, "ell");
    }
    
    #[test]
    fn test_class_match() {
        let result = match_pattern("abc123", "%d+").unwrap();
        assert!(result.is_some());
        assert_eq!(result.unwrap().matched, "123");
    }
    
    #[test]
    fn test_capture() {
        let result = match_pattern("hello world", "(%a+) (%a+)").unwrap();
        assert!(result.is_some());
        let r = result.unwrap();
        assert_eq!(r.captures.len(), 2);
        assert_eq!(r.captures[0], "hello");
        assert_eq!(r.captures[1], "world");
    }
    
    #[test]
    fn test_gsub() {
        let (result, count) = gsub("hello world", "o", "0", None).unwrap();
        assert_eq!(result, "hell0 w0rld");
        assert_eq!(count, 2);
    }
}
