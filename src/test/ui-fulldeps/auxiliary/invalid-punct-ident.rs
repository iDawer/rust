// Copyright 2017 The Rust Project Developers. See the COPYRIGHT
// file at the top-level directory of this distribution and at
// http://rust-lang.org/COPYRIGHT.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

// force-host
// no-prefer-dynamic

#![feature(proc_macro)]
#![crate_type = "proc-macro"]

extern crate proc_macro;
use proc_macro::*;

#[proc_macro]
pub fn invalid_punct(_: TokenStream) -> TokenStream {
    TokenTree::from(Punct::new('`', Spacing::Alone)).into()
}

#[proc_macro]
pub fn invalid_ident(_: TokenStream) -> TokenStream {
    TokenTree::from(Ident::new("*", Span::call_site())).into()
}

#[proc_macro]
pub fn invalid_raw_ident(_: TokenStream) -> TokenStream {
    TokenTree::from(Ident::new_raw("self", Span::call_site())).into()
}

#[proc_macro]
pub fn lexer_failure(_: TokenStream) -> TokenStream {
    "a b ) c".parse().expect("parsing failed without panic")
}
