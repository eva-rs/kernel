use std::collections::BTreeSet;
use syn::{
    Expr, Ident, LitStr, Token,
    parse::{Parse, ParseStream},
};

pub struct Input {
    pub id: LitStr,
    pub plural: Option<LitStr>,
    pub context: Option<LitStr>,
    pub args: Vec<(Ident, Expr)>,
}
impl Parse for Input {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        let id = input.parse()?;
        let mut this = Self {
            id,
            plural: None,
            context: None,
            args: Vec::new(),
        };
        let mut seen = BTreeSet::new();
        while !input.is_empty() {
            input.parse::<Token![,]>()?;
            if input.is_empty() {
                break;
            }
            let name: Ident = input.parse()?;
            if !seen.insert(name.to_string()) {
                return Err(syn::Error::new(name.span(), "duplicate message parameter"));
            }
            input.parse::<Token![=]>()?;
            match name.to_string().as_str() {
                "context" => this.context = Some(input.parse()?),
                "plural" => this.plural = Some(input.parse()?),
                _ => this.args.push((name, input.parse()?)),
            }
        }
        this.validate()?;
        Ok(this)
    }
}
impl Input {
    pub fn validate(&self) -> syn::Result<()> {
        let fail = |e| syn::Error::new(self.id.span(), e);
        let mut required =
            eva_protocol::types::text::placeholders(&self.id.value()).map_err(fail)?;
        if let Some(plural) = &self.plural {
            required
                .extend(eva_protocol::types::text::placeholders(&plural.value()).map_err(fail)?);
            required.insert("count".into());
        }
        let provided: BTreeSet<_> = self.args.iter().map(|(name, _)| name.to_string()).collect();
        if required != provided {
            return Err(fail(format!(
                "message parameters do not match placeholders: expected {required:?}, got {provided:?}"
            )));
        }
        Ok(())
    }
}
