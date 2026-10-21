//! `#[derive(veil::Schema)]`: JSON Schema (`ApiSchema`) and automatic validation (`Validate`).

use crate::util::{doc_text, lit_str, option_inner};
use proc_macro2::TokenStream;
use quote::quote;
use syn::{spanned::Spanned, Attribute, Data, DeriveInput, Error, Expr, Fields, Lit, LitStr, Type};

/// The serde attributes of a field (or container) that affect the schema.
#[derive(Default)]
struct SerdeAttrs {
    rename: Option<String>,
    rename_all: Option<String>,
    default: bool,
    skip: bool,
    flatten: bool,
}

fn serde_attrs(attrs: &[Attribute]) -> SerdeAttrs {
    let mut out = SerdeAttrs::default();
    for a in attrs.iter().filter(|a| a.path().is_ident("serde")) {
        let _ = a.parse_nested_meta(|meta| {
            let key = meta
                .path
                .get_ident()
                .map(|i| i.to_string())
                .unwrap_or_default();
            match key.as_str() {
                "rename" | "rename_all" => {
                    if let Ok(v) = meta.value() {
                        if let Ok(s) = v.parse::<LitStr>() {
                            if key == "rename" {
                                out.rename = Some(s.value());
                            } else {
                                out.rename_all = Some(s.value());
                            }
                        }
                    } else {
                        // rename(deserialize = "..") 形式は deserialize 側だけ見る
                        let _ = meta.parse_nested_meta(|m| {
                            if m.path.is_ident("deserialize") {
                                if let Ok(s) = m.value()?.parse::<LitStr>() {
                                    if key == "rename" {
                                        out.rename = Some(s.value());
                                    } else {
                                        out.rename_all = Some(s.value());
                                    }
                                }
                            } else if let Ok(v) = m.value() {
                                let _ = v.parse::<LitStr>();
                            }
                            Ok(())
                        });
                    }
                }
                "default" => {
                    out.default = true;
                    if let Ok(v) = meta.value() {
                        let _ = v.parse::<LitStr>();
                    }
                }
                "skip" | "skip_deserializing" => out.skip = true,
                "flatten" => out.flatten = true,
                _ => {
                    // 値付きの未知キーは読み飛ばす
                    if let Ok(v) = meta.value() {
                        let _ = v.parse::<Expr>();
                    }
                }
            }
            Ok(())
        });
    }
    out
}

/// Applies serde's `rename_all`.
fn rename(name: &str, rule: Option<&str>) -> String {
    let words: Vec<String> = split_words(name);
    match rule {
        Some("lowercase") => name.to_lowercase(),
        Some("UPPERCASE") => name.to_uppercase(),
        Some("snake_case") => words.join("_").to_lowercase(),
        Some("SCREAMING_SNAKE_CASE") => words.join("_").to_uppercase(),
        Some("kebab-case") => words.join("-").to_lowercase(),
        Some("SCREAMING-KEBAB-CASE") => words.join("-").to_uppercase(),
        Some("camelCase") => {
            let mut out = String::new();
            for (i, w) in words.iter().enumerate() {
                if i == 0 {
                    out.push_str(&w.to_lowercase());
                } else {
                    out.push_str(&capitalize(w));
                }
            }
            out
        }
        Some("PascalCase") => words.iter().map(|w| capitalize(w)).collect(),
        _ => name.to_string(),
    }
}

fn capitalize(w: &str) -> String {
    let lower = w.to_lowercase();
    let mut c = lower.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}

/// Splits both `snake_case` and `PascalCase` into words.
fn split_words(name: &str) -> Vec<String> {
    let mut words = Vec::new();
    for part in name.split('_').filter(|p| !p.is_empty()) {
        let mut cur = String::new();
        let chars: Vec<char> = part.chars().collect();
        for (i, &ch) in chars.iter().enumerate() {
            if ch.is_uppercase()
                && !cur.is_empty()
                && (chars[i - 1].is_lowercase()
                    || chars.get(i + 1).map(|n| n.is_lowercase()).unwrap_or(false))
            {
                words.push(std::mem::take(&mut cur));
            }
            cur.push(ch);
        }
        if !cur.is_empty() {
            words.push(cur);
        }
    }
    words
}

/// The contents of `#[validate(...)]` and `#[schema(...)]`.
#[derive(Default)]
struct Rules {
    min: Option<f64>,
    max: Option<f64>,
    exclusive_min: Option<f64>,
    exclusive_max: Option<f64>,
    len_min: Option<u64>,
    len_max: Option<u64>,
    len_equal: Option<u64>,
    email: bool,
    url: bool,
    nested: bool,
    required: bool,
    custom: Vec<syn::Path>,
    description: Option<String>,
    example: Option<TokenStream>,
    format: Option<String>,
    pattern: Option<String>,
}

fn num(e: &Expr) -> syn::Result<f64> {
    match e {
        Expr::Lit(l) => match &l.lit {
            Lit::Int(i) => i.base10_parse::<f64>(),
            Lit::Float(f) => f.base10_parse::<f64>(),
            _ => Err(Error::new(e.span(), "expected a number")),
        },
        Expr::Unary(u) if matches!(u.op, syn::UnOp::Neg(_)) => Ok(-num(&u.expr)?),
        _ => Err(Error::new(e.span(), "expected a number literal")),
    }
}

fn uint(e: &Expr) -> syn::Result<u64> {
    let v = num(e)?;
    if v < 0.0 || v.fract() != 0.0 {
        return Err(Error::new(e.span(), "expected a non-negative integer"));
    }
    Ok(v as u64)
}

fn parse_rules(attrs: &[Attribute]) -> syn::Result<Rules> {
    let mut r = Rules::default();
    for a in attrs {
        if a.path().is_ident("validate") {
            a.parse_nested_meta(|meta| {
                let key = meta
                    .path
                    .get_ident()
                    .map(|i| i.to_string())
                    .unwrap_or_default();
                match key.as_str() {
                    "range" => meta.parse_nested_meta(|m| {
                        let k = m
                            .path
                            .get_ident()
                            .map(|i| i.to_string())
                            .unwrap_or_default();
                        let v: Expr = m.value()?.parse()?;
                        let n = num(&v)?;
                        match k.as_str() {
                            "min" => r.min = Some(n),
                            "max" => r.max = Some(n),
                            "exclusive_min" => r.exclusive_min = Some(n),
                            "exclusive_max" => r.exclusive_max = Some(n),
                            _ => {
                                return Err(
                                    m.error("expected min, max, exclusive_min or exclusive_max")
                                )
                            }
                        }
                        Ok(())
                    }),
                    "length" => meta.parse_nested_meta(|m| {
                        let k = m
                            .path
                            .get_ident()
                            .map(|i| i.to_string())
                            .unwrap_or_default();
                        let v: Expr = m.value()?.parse()?;
                        let n = uint(&v)?;
                        match k.as_str() {
                            "min" => r.len_min = Some(n),
                            "max" => r.len_max = Some(n),
                            "equal" => r.len_equal = Some(n),
                            _ => return Err(m.error("expected min, max or equal")),
                        }
                        Ok(())
                    }),
                    "email" => {
                        r.email = true;
                        Ok(())
                    }
                    "url" => {
                        r.url = true;
                        Ok(())
                    }
                    "nested" => {
                        r.nested = true;
                        Ok(())
                    }
                    "required" => {
                        r.required = true;
                        Ok(())
                    }
                    "custom" => {
                        // custom = "path" / custom(function = "path")
                        if let Ok(v) = meta.value() {
                            let s: LitStr = v.parse()?;
                            r.custom.push(s.parse()?);
                            Ok(())
                        } else {
                            meta.parse_nested_meta(|m| {
                                if m.path.is_ident("function") {
                                    let s: LitStr = m.value()?.parse()?;
                                    r.custom.push(s.parse()?);
                                } else if let Ok(v) = m.value() {
                                    let _: Expr = v.parse()?;
                                }
                                Ok(())
                            })
                        }
                    }
                    _ => {
                        // validator の他の検査（regex 等）は受け付けて無視する（validator の derive と併用するため）
                        if let Ok(v) = meta.value() {
                            let _: Expr = v.parse()?;
                        } else if meta.input.peek(syn::token::Paren) {
                            let content;
                            syn::parenthesized!(content in meta.input);
                            let _: TokenStream = content.parse()?;
                        }
                        Ok(())
                    }
                }
            })?;
        } else if a.path().is_ident("schema") {
            a.parse_nested_meta(|meta| {
                let key = meta
                    .path
                    .get_ident()
                    .map(|i| i.to_string())
                    .unwrap_or_default();
                let v: Expr = meta.value()?.parse()?;
                match key.as_str() {
                    "description" => r.description = lit_str(&v),
                    "format" => r.format = lit_str(&v),
                    "pattern" => r.pattern = lit_str(&v),
                    "example" => {
                        r.example = Some(quote!(::veil::api::__private::serde_json::json!(#v)))
                    }
                    _ => return Err(meta.error("expected description, example, format or pattern")),
                }
                Ok(())
            })?;
        }
    }
    Ok(r)
}

fn opt_f64(v: Option<f64>) -> TokenStream {
    match v {
        Some(n) => quote!(Some(#n)),
        None => quote!(None),
    }
}

fn opt_usize(v: Option<u64>) -> TokenStream {
    match v {
        Some(n) => {
            let n = n as usize;
            quote!(Some(#n))
        }
        None => quote!(None),
    }
}

/// The constraints (JSON Schema keywords) attached to a field's schema.
fn constraint_inserts(r: &Rules, desc: &Option<String>) -> TokenStream {
    let mut ins = Vec::new();
    let put = |k: &str, v: TokenStream| quote!(__c.insert(#k.into(), ::veil::api::__private::serde_json::json!(#v)););
    // 整数で書かれた境界は JSON でも整数で出す（`150` を `150.0` にしない）
    let numlit = |v: f64| -> TokenStream {
        if v.fract() == 0.0 && v.abs() < 9.0e15 {
            let i = v as i64;
            quote!(#i)
        } else {
            quote!(#v)
        }
    };
    if let Some(v) = r.min {
        ins.push(put("minimum", numlit(v)));
    }
    if let Some(v) = r.max {
        ins.push(put("maximum", numlit(v)));
    }
    if let Some(v) = r.exclusive_min {
        ins.push(put("exclusiveMinimum", numlit(v)));
    }
    if let Some(v) = r.exclusive_max {
        ins.push(put("exclusiveMaximum", numlit(v)));
    }
    if let Some(v) = r.len_equal {
        ins.push(quote! {
            if __kind_is_array { __c.insert("minItems".into(), #v.into()); __c.insert("maxItems".into(), #v.into()); }
            else { __c.insert("minLength".into(), #v.into()); __c.insert("maxLength".into(), #v.into()); }
        });
    }
    if let Some(v) = r.len_min {
        ins.push(quote! {
            __c.insert(if __kind_is_array { "minItems" } else { "minLength" }.into(), #v.into());
        });
    }
    if let Some(v) = r.len_max {
        ins.push(quote! {
            __c.insert(if __kind_is_array { "maxItems" } else { "maxLength" }.into(), #v.into());
        });
    }
    if r.email {
        ins.push(put("format", quote!("email")));
    }
    if r.url {
        ins.push(put("format", quote!("uri")));
    }
    if let Some(f) = &r.format {
        ins.push(put("format", quote!(#f)));
    }
    if let Some(p) = &r.pattern {
        ins.push(put("pattern", quote!(#p)));
    }
    if let Some(d) = desc {
        ins.push(put("description", quote!(#d)));
    }
    if let Some(e) = &r.example {
        ins.push(
            quote!(__c.insert("examples".into(), ::veil::api::__private::serde_json::json!([#e]));),
        );
    }
    quote!(#(#ins)*)
}

fn is_vec_like(ty: &Type) -> bool {
    let t = option_inner(ty).unwrap_or(ty);
    matches!(
        crate::util::last_ident(t).as_deref(),
        Some("Vec") | Some("HashSet") | Some("BTreeSet") | Some("VecDeque")
    ) || matches!(t, Type::Array(_) | Type::Slice(_))
}

pub fn expand(item: TokenStream) -> syn::Result<TokenStream> {
    let input: DeriveInput = syn::parse2(item)?;
    let ident = &input.ident;
    let name = ident.to_string();
    let container = serde_attrs(&input.attrs);
    let container_doc = doc_text(&input.attrs);
    let (impl_g, ty_g, where_g) = input.generics.split_for_impl();
    // 型パラメータには ApiSchema を要求する
    let mut schema_where = where_g.cloned().unwrap_or_else(|| syn::parse_quote!(where));
    for p in input.generics.type_params() {
        let id = &p.ident;
        schema_where
            .predicates
            .push(syn::parse_quote!(#id: ::veil::api::ApiSchema));
    }
    let desc_insert = match &container_doc {
        Some(d) => quote!(__obj.insert("description".into(), #d.into());),
        None => quote!(),
    };

    let (schema_body, validate_body) = match &input.data {
        Data::Struct(s) => match &s.fields {
            Fields::Named(fields) => {
                let mut props = Vec::new();
                let mut checks = Vec::new();
                for f in &fields.named {
                    let sa = serde_attrs(&f.attrs);
                    if sa.skip {
                        continue;
                    }
                    let fident = f.ident.as_ref().unwrap();
                    let raw = fident.to_string();
                    let raw = raw.trim_start_matches("r#");
                    let json_name = sa
                        .rename
                        .clone()
                        .unwrap_or_else(|| rename(raw, container.rename_all.as_deref()));
                    let fty = &f.ty;
                    let rules = parse_rules(&f.attrs)?;
                    let desc = rules.description.clone().or_else(|| doc_text(&f.attrs));
                    let kind_is_array = is_vec_like(fty);
                    let inserts = constraint_inserts(&rules, &desc);
                    let optional_by_type = option_inner(fty).is_some();
                    let required =
                        !(optional_by_type || sa.default || container.default) || rules.required;
                    if sa.flatten {
                        props.push(quote! {
                            {
                                let __s = <#fty as ::veil::api::ApiSchema>::schema(defs);
                                let __resolved = match __s.get("$ref").and_then(|r| r.as_str()) {
                                    Some(r) => defs.get(r.trim_start_matches("#/components/schemas/")).cloned().unwrap_or_default(),
                                    None => __s,
                                };
                                if let Some(p) = __resolved.get("properties").and_then(|p| p.as_object()) {
                                    for (k, v) in p { __props.insert(k.clone(), v.clone()); }
                                }
                                if let Some(r) = __resolved.get("required").and_then(|r| r.as_array()) {
                                    __required.extend(r.iter().cloned());
                                }
                            }
                        });
                    } else {
                        props.push(quote! {
                            {
                                let __kind_is_array = #kind_is_array;
                                let _ = __kind_is_array;
                                let mut __c = ::veil::api::__private::serde_json::Map::new();
                                #inserts
                                let __s = ::veil::api::__private::with_constraints(
                                    <#fty as ::veil::api::ApiSchema>::schema(defs), __c);
                                __props.insert(#json_name.into(), __s);
                                if #required {
                                    __required.push(#json_name.into());
                                }
                            }
                        });
                    }
                    // 検証
                    let mut fchecks = Vec::new();
                    if rules.min.is_some()
                        || rules.max.is_some()
                        || rules.exclusive_min.is_some()
                        || rules.exclusive_max.is_some()
                    {
                        let (a, b, c, d) = (
                            opt_f64(rules.min),
                            opt_f64(rules.max),
                            opt_f64(rules.exclusive_min),
                            opt_f64(rules.exclusive_max),
                        );
                        fchecks.push(quote! {
                            ::veil::api::__private::check_range(
                                ::veil::api::__private::AsNumber::as_number(&self.#fident), #a, #b, #c, #d, loc, errors);
                        });
                    }
                    if rules.len_min.is_some()
                        || rules.len_max.is_some()
                        || rules.len_equal.is_some()
                    {
                        let (a, b, c) = (
                            opt_usize(rules.len_min),
                            opt_usize(rules.len_max),
                            opt_usize(rules.len_equal),
                        );
                        fchecks.push(quote! {
                            ::veil::api::__private::check_length(
                                ::veil::api::__private::AsLength::as_length(&self.#fident), #a, #b, #c, loc, errors);
                        });
                    }
                    if rules.email {
                        fchecks.push(quote! {
                            ::veil::api::__private::check_email(::veil::api::__private::AsText::as_text(&self.#fident), loc, errors);
                        });
                    }
                    if rules.url {
                        fchecks.push(quote! {
                            ::veil::api::__private::check_url(::veil::api::__private::AsText::as_text(&self.#fident), loc, errors);
                        });
                    }
                    if rules.required && optional_by_type {
                        fchecks.push(quote! {
                            if self.#fident.is_none() { errors.push(loc, "missing", "Field required"); }
                        });
                    }
                    if rules.nested {
                        fchecks.push(quote! {
                            ::veil::api::Validate::validate_at(&self.#fident, loc, errors);
                        });
                    }
                    for c in &rules.custom {
                        fchecks.push(quote! {
                            if let Err(e) = #c(&self.#fident) {
                                errors.push(loc, "value_error", e.to_string());
                            }
                        });
                    }
                    if !fchecks.is_empty() {
                        checks.push(quote! {
                            loc.push(::veil::api::__private::serde_json::Value::from(#json_name));
                            #(#fchecks)*
                            loc.pop();
                        });
                    }
                }
                (
                    quote! {
                        let mut __props = ::veil::api::__private::serde_json::Map::new();
                        let mut __required: ::veil::api::__private::Vec<::veil::api::__private::serde_json::Value> = ::veil::api::__private::Vec::new();
                        #(#props)*
                        let mut __obj = ::veil::api::__private::serde_json::Map::new();
                        __obj.insert("type".into(), "object".into());
                        __obj.insert("properties".into(), ::veil::api::__private::serde_json::Value::Object(__props));
                        if !__required.is_empty() {
                            __obj.insert("required".into(), ::veil::api::__private::serde_json::Value::Array(__required));
                        }
                        #desc_insert
                        ::veil::api::__private::serde_json::Value::Object(__obj)
                    },
                    quote!(#(#checks)*),
                )
            }
            Fields::Unnamed(u) if u.unnamed.len() == 1 => {
                let fty = &u.unnamed.first().unwrap().ty;
                (
                    quote!(<#fty as ::veil::api::ApiSchema>::schema(defs)),
                    quote!(),
                )
            }
            Fields::Unnamed(u) => {
                let tys: Vec<&Type> = u.unnamed.iter().map(|f| &f.ty).collect();
                let n = tys.len();
                (
                    quote! {
                        ::veil::api::__private::serde_json::json!({
                            "type": "array",
                            "prefixItems": [#(<#tys as ::veil::api::ApiSchema>::schema(defs)),*],
                            "minItems": #n, "maxItems": #n,
                        })
                    },
                    quote!(),
                )
            }
            Fields::Unit => (
                quote!(::veil::api::__private::serde_json::json!({"type": "null"})),
                quote!(),
            ),
        },
        Data::Enum(e) => {
            let all_unit = e.variants.iter().all(|v| matches!(v.fields, Fields::Unit));
            if all_unit {
                let names: Vec<String> = e
                    .variants
                    .iter()
                    .filter(|v| !serde_attrs(&v.attrs).skip)
                    .map(|v| {
                        let sa = serde_attrs(&v.attrs);
                        sa.rename.unwrap_or_else(|| {
                            rename(&v.ident.to_string(), container.rename_all.as_deref())
                        })
                    })
                    .collect();
                (
                    quote! {
                        let mut __obj = ::veil::api::__private::serde_json::Map::new();
                        __obj.insert("type".into(), "string".into());
                        __obj.insert("enum".into(), ::veil::api::__private::serde_json::json!([#(#names),*]));
                        #desc_insert
                        ::veil::api::__private::serde_json::Value::Object(__obj)
                    },
                    quote!(),
                )
            } else {
                // データを持つ列挙はタグ付けの形が serde 属性次第なので、汎用（任意の値）として載せる。
                (
                    quote! {
                        let mut __obj = ::veil::api::__private::serde_json::Map::new();
                        #desc_insert
                        ::veil::api::__private::serde_json::Value::Object(__obj)
                    },
                    quote!(),
                )
            }
        }
        Data::Union(u) => {
            return Err(Error::new(
                u.union_token.span(),
                "Schema cannot be derived for unions",
            ))
        }
    };

    Ok(quote! {
        impl #impl_g ::veil::api::ApiSchema for #ident #ty_g #schema_where {
            fn schema(defs: &mut ::veil::api::Definitions) -> ::veil::api::__private::serde_json::Value {
                if !defs.contains_key(#name) {
                    // 再帰型に備えて先に場所を取る
                    defs.insert(#name.into(), ::veil::api::__private::serde_json::Value::Null);
                    let __schema = { #schema_body };
                    defs.insert(#name.into(), __schema);
                }
                ::veil::api::__private::schema_ref(#name)
            }
        }

        impl #impl_g ::veil::api::Validate for #ident #ty_g #where_g {
            #[allow(unused_variables)]
            fn validate_at(
                &self,
                loc: &mut ::veil::api::__private::Vec<::veil::api::__private::serde_json::Value>,
                errors: &mut ::veil::api::ValidationErrors,
            ) {
                #validate_body
            }
        }
    })
}
