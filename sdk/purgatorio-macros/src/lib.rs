//! `#[purgatorio::main]`: turns
//! `fn main(ctx: Context, args: Vec<String>) -> Result<(), Error>` into the
//! command's entry point, and embeds the `purgatorio.abi` custom section.

use proc_macro::{Delimiter, Group, Ident, TokenStream, TokenTree};

const INNER: &str = "__purgatorio_main";

/// Marks a command's `init`-style entry point. The function receives the
/// [`Context`] and argv (with argv\[0\] the command name); an `Err` is
/// printed as `argv0: msg` on fd 2 and becomes a `fail:` exit status.
#[proc_macro_attribute]
pub fn main(attr: TokenStream, item: TokenStream) -> TokenStream {
    if !attr.is_empty() {
        return error("#[purgatorio::main] takes no arguments");
    }
    let mut renamed = TokenStream::new();
    let mut tokens = item.into_iter().peekable();
    let mut found = false;
    while let Some(tt) = tokens.next() {
        let is_fn = matches!(&tt, TokenTree::Ident(i) if i.to_string() == "fn");
        renamed.extend([tt]);
        if is_fn && !found {
            match tokens.next() {
                Some(TokenTree::Ident(name)) => {
                    renamed.extend([TokenTree::Ident(Ident::new(INNER, name.span()))]);
                    found = true;
                }
                _ => return error("expected a function name"),
            }
        }
    }
    if !found {
        return error("#[purgatorio::main] must be applied to a function");
    }
    let header: TokenStream = concat!(
        "#[cfg(target_family = \"wasm\")]",
        "#[used]",
        "#[unsafe(link_section = \"purgatorio.abi\")]",
        "static __PURGATORIO_ABI: [u8; ::purgatorio::ABI_LEN] = ::purgatorio::ABI_BYTES;",
    )
    .parse()
    .expect("static tokens");
    let call: TokenStream = format!("::purgatorio::rt::run({INNER});")
        .parse()
        .expect("call tokens");
    let mut body = TokenStream::new();
    body.extend(header);
    body.extend(renamed);
    body.extend(call);
    let mut out: TokenStream = "fn main()".parse().expect("signature tokens");
    out.extend([TokenTree::Group(Group::new(Delimiter::Brace, body))]);
    out
}

fn error(msg: &str) -> TokenStream {
    format!("::core::compile_error!({msg:?});")
        .parse()
        .expect("error tokens")
}
