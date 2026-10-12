//! マクロ共通の小物。

use syn::{Attribute, Expr, ExprLit, GenericArgument, Lit, PathArguments, Type};

/// doc コメント（`///`）を連結する。
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

/// 型の最後のセグメント名（`veil::api::Json<T>` → `Json`）。
pub fn last_ident(ty: &Type) -> Option<String> {
    match ty {
        Type::Path(p) => p.path.segments.last().map(|s| s.ident.to_string()),
        Type::Reference(r) => last_ident(&r.elem),
        Type::Group(g) => last_ident(&g.elem),
        Type::Paren(p) => last_ident(&p.elem),
        _ => None,
    }
}

/// `Name<T>` の最初の型引数。
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

/// 括弧・グループを外す。
pub fn strip(ty: &Type) -> &Type {
    match ty {
        Type::Group(g) => strip(&g.elem),
        Type::Paren(p) => strip(&p.elem),
        other => other,
    }
}

/// `Option<T>` なら `T`。
pub fn option_inner(ty: &Type) -> Option<&Type> {
    if last_ident(ty).as_deref() == Some("Option") {
        first_generic(ty)
    } else {
        None
    }
}

/// パスパターンの構文検査とパラメータ名の取り出し（本体の `PathPattern::parse` と同じ規則）。
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

/// 文字列リテラルの値。
pub fn lit_str(e: &Expr) -> Option<String> {
    match e {
        Expr::Lit(ExprLit {
            lit: Lit::Str(s), ..
        }) => Some(s.value()),
        _ => None,
    }
}
