// Copyright 2018 The HuggingFace Inc. team.
// Copyright 2019 Guillaume Becquin
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//     http://www.apache.org/licenses/LICENSE-2.0
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

pub mod preprocessing;

pub use crate::preprocessing::error;
pub use crate::preprocessing::tokenizer::albert_tokenizer::AlbertTokenizer;
pub use crate::preprocessing::tokenizer::base_tokenizer::{
    MultiThreadedTokenizer, TokenizedInput, Tokenizer, TruncationStrategy,
};
pub use crate::preprocessing::tokenizer::bert_tokenizer::BertTokenizer;
pub use crate::preprocessing::tokenizer::ctrl_tokenizer::CtrlTokenizer;
pub use crate::preprocessing::tokenizer::gpt2_tokenizer::Gpt2Tokenizer;
pub use crate::preprocessing::tokenizer::openai_gpt_tokenizer::OpenAiGptTokenizer;
pub use crate::preprocessing::tokenizer::roberta_tokenizer::RobertaTokenizer;
pub use crate::preprocessing::tokenizer::sentence_piece_tokenizer::SentencePieceTokenizer;
pub use crate::preprocessing::tokenizer::xlnet_tokenizer::XLNetTokenizer;
pub use crate::preprocessing::vocab::base_vocab::Vocab;
pub use preprocessing::tokenizer::bert_tokenizer;
pub use preprocessing::tokenizer::tokenization_utils;
pub use preprocessing::vocab::{
    base_vocab::BaseVocab, bert_vocab::BertVocab, gpt2_vocab::Gpt2Vocab,
    openai_gpt_vocab::OpenAiGptVocab, roberta_vocab::RobertaVocab,
    xlm_roberta_vocab::XLMRobertaVocab, xlnet_vocab::XLNetVocab,
};

#[macro_use]
extern crate lazy_static;
