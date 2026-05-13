mod tokenizer;
mod tree_builder;

use crate::dom::Dom;
use tokenizer::Tokenizer;

pub fn parse(input: &str) -> Dom {
    tree_builder::build(Tokenizer::new(input).tokenize())
}
