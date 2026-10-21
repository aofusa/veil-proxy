//! Small helpers shared by the macros.

use syn::{Attribute, Expr, ExprLit, GenericArgument, Lit, PathArguments, Type};

/// Joins the doc comments (`///`).
pub fn doc_text(attrs: &[Attribute]) -> Option<String> {
    let mut lines = Vec::new();
    for a in attrs {
        if !a.path().is_ident("doc") {
            continue;
        }
        if let syn::Meta::NameValue(nv) = &a.meta {
            if let Expr::Lit(ExprLit {
                lit: Lit::Str(s), ..
            }) = &nv.value
            {
                lines.push(s.value().trim().to_string());
            }
        }
    }
    let text = lines.join("\n").trim().to_string();
    if text.is_empty() {
        None
    } else {
        Some(text)
    }
}

/// The name of the last segment of a type (`veil::api::Json<T>` becomes `Json`).
pub fn last_ident(ty: &Type) -> Option<String> {
    match ty {
        Type::Path(p) => p.path.segments.last().map(|s| s.ident.to_string()),
        Type::Reference(r) => last_ident(&r.elem),
        Type::Group(g) => last_ident(&g.elem),
        Type::Paren(p) => last_ident(&p.elem),
        _ => None,
    }
}

/// The first type argument of `Name<T>`.
pub fn first_generic(ty: &Type) -> Option<&Type> {
    let Type::Path(p) = strip(ty) else {
        return None;
    };
    let seg = p.path.segments.last()?;
    let PathArguments::AngleBracketed(args) = &seg.arguments else {
        return None;
    };
    args.args.iter().find_map(|a| match a {
        GenericArgument::Type(t) => Some(t),
        _ => None,
    })
}

/// Strips parentheses and groups.
pub fn strip(ty: &Type) -> &Type {
    match ty {
        Type::Group(g) => strip(&g.elem),
        Type::Paren(p) => strip(&p.elem),
        other => other,
    }
}

/// `T` if the type is `Option<T>`.
pub fn option_inner(ty: &Type) -> Option<&Type> {
    if last_ident(ty).as_deref() == Some("Option") {
        first_generic(ty)
    } else {
        None
    }
}

/// Checks the syntax of a path pattern and extracts the parameter names (the same rules as `PathPattern::parse` in the main crate).
pub fn parse_path_params(pattern: &str) -> Result<Vec<String>, String> {
    if !pattern.starts_with('/') {
        return Err(format!("path '{}' must start with '/'", pattern));
    }
    let body = &pattern[1..];
    let mut names = Vec::new();
    if body.is_empty() {
        return Ok(names);
    }
    let parts: Vec<&str> = body.split('/').collect();
    let last = parts.len() - 1;
    for (i, part) in parts.iter().enumerate() {
        if let Some(inner) = part.strip_prefix('{').and_then(|p| p.strip_suffix('}')) {
            let (catch_all, name) = match inner.strip_prefix('*') {
                Some(n) => (true, n),
                None => (false, inner),
            };
            if name.is_empty()
                || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
                || name.starts_with(|c: char| c.is_ascii_digit())
            {
                return Err(format!(
                    "invalid parameter name '{}' in '{}'",
                    inner, pattern
                ));
            }
            if catch_all && i != last {
                return Err(format!(
                    "catch-all '{{*{}}}' must be the last segment in '{}'",
                    name, pattern
                ));
            }
            if names.iter().any(|n| n == name) {
                return Err(format!("duplicate parameter '{}' in '{}'", name, pattern));
            }
            names.push(name.to_string());
        } else if part.contains('{') || part.contains('}') {
            return Err(format!(
                "a parameter must occupy a whole segment ('{}' in '{}')",
                part, pattern
            ));
        } else if part.is_empty() && i != last {
            return Err(format!("empty segment in '{}'", pattern));
        }
    }
    Ok(names)
}

/// The value of a string literal.
pub fn lit_str(e: &Expr) -> Option<String> {
    match e {
        Expr::Lit(ExprLit {
            lit: Lit::Str(s), ..
        }) => Some(s.value()),
        _ => None,
    }
}
