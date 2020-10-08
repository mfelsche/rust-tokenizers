// Copyright 2018 The Open AI Team Authors, The Google AI Language Team Authors
// Copyright 2018 The HuggingFace Inc. team.
// Copyright 2019-2020 Guillaume Becquin
// Copyright 2020 Maarten van Gompel
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//     http://www.apache.org/licenses/LICENSE-2.0
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use crate::preprocessing::error::TokenizerError;
use crate::preprocessing::tokenizer::tokenization_utils::{
    split_on_punct, split_on_special_tokens, strip_accents, tokenize_cjk_chars, truncate_sequences,
    whitespace_tokenize,
};
use crate::preprocessing::vocab::base_vocab::Vocab;
use crate::tokenization_utils::lowercase;
use itertools::Itertools;
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

#[derive(Debug)]
pub enum TruncationStrategy {
    LongestFirst,
    OnlyFirst,
    OnlySecond,
    DoNotTruncate,
}

pub type OffsetSize = u32;

#[derive(Debug, PartialEq, PartialOrd, Clone, Copy, Serialize, Deserialize)]
///Offset information (in unicode points) to relate a token back to its original input string
pub struct Offset {
    pub begin: OffsetSize,
    pub end: OffsetSize,
}

#[derive(Debug, PartialEq, PartialOrd, Clone, Copy, Serialize, Deserialize)]
pub enum Mask {
    ///The token has no particular mask. This is the default situation. It may indicate that further processing can be done on a token.
    None,
    ///the token represents a whitespace (in any shape or form)
    Whitespace,
    ///the token represents punctuation (in any shape or form)
    Punctuation,
    ///the token represents a single Chinese/Japanese/Korean character (including kana and hangul)
    CJK,
    ///the token is a special marker (such as a separator marker, a class marker, etc)
    Special,
    ///the token is the begin in a series of subtokens, the offset refers specifically to the subtoken. Subsequent tokens in this sequence will carry the 'Continuation' mask
    Begin,
    ///the token is the continuation of the previous token, the offset refers specifically to the subtoken. All but the first subtoken in a sequence carry this mask (the first carries 'Begin'). (this is the reverse of Mask::Unfinished)
    Continuation,
    ///the token is the start of a token but not finished yet. All but the last subtoken in the a token sequence carry this mask. This is the reverse of Mask::Continuation.
    Unfinished,
    ///The token is out of vocabulary, it is unknown by the tokenizer and it will decode to unknown. Tokens that can be decoded properly (but may still be out of vocabulary) should not set this.
    Unknown,
}

impl Default for Mask {
    fn default() -> Mask {
        Mask::None
    }
}

pub trait TokenTrait {
    fn offset(&self) -> Option<Offset>;
    fn mask(&self) -> Mask;
    fn as_str(&self) -> &str;
}

#[derive(Debug, PartialEq, Clone, Copy)]
///A token that references the original text
pub struct TokenRef<'a> {
    pub text: &'a str,
    pub offset: Offset,
    pub reference_offsets: &'a [OffsetSize],
    pub mask: Mask,
}

impl<'a> TokenRef<'a> {
    pub fn new(text: &'a str, offsets: &'a [OffsetSize]) -> TokenRef<'a> {
        TokenRef {
            text,
            offset: Offset {
                begin: 0,
                end: offsets.len() as OffsetSize,
            },
            reference_offsets: offsets,
            mask: Mask::None,
        }
    }

    pub fn to_owned(self) -> Token {
        //not a real implementation of ToOwned because that can't work in the current setup
        Token::from(self)
    }
}

impl<'a> TokenTrait for TokenRef<'a> {
    fn offset(&self) -> Option<Offset> {
        self.offset.into_option()
    }

    fn mask(&self) -> Mask {
        self.mask
    }

    fn as_str(&self) -> &str {
        self.text
    }
}

impl TokenTrait for Token {
    fn offset(&self) -> Option<Offset> {
        self.offset.into_option()
    }

    fn mask(&self) -> Mask {
        self.mask
    }

    fn as_str(&self) -> &str {
        self.text.as_str()
    }
}

impl<'a> From<&'a Token> for TokenRef<'a> {
    fn from(other: &'a Token) -> Self {
        TokenRef {
            text: other.text.as_str(),
            offset: other.offset,
            reference_offsets: &other.reference_offsets,
            mask: other.mask,
        }
    }
}

impl From<&str> for Token {
    fn from(text: &str) -> Self {
        Token::new(text.to_owned())
    }
}

impl<'a> From<TokenRef<'a>> for Token {
    fn from(other: TokenRef<'a>) -> Self {
        Token {
            text: other.text.to_owned(),
            offset: other.offset,
            reference_offsets: other.reference_offsets.to_vec(),
            mask: other.mask,
        }
    }
}

/// # ConsolidatedTokenIterator
///
/// This iterator loops over collections of tokens (i.e. things that implement `TokenTrait`)
/// and groups all subtokens that belong together (forming a word or something similar).
pub struct ConsolidatedTokenIterator<'a, T>
where
    T: TokenTrait,
{
    pub tokens: &'a Vec<T>,
    pub begin: usize,
    pub cursor: usize,
}

impl<'a, T> ConsolidatedTokenIterator<'a, T>
where
    T: TokenTrait,
{
    pub fn new(tokens: &'a Vec<T>) -> Self {
        ConsolidatedTokenIterator {
            tokens,
            begin: 0,
            cursor: 0,
        }
    }
}

impl<'a, T> Iterator for ConsolidatedTokenIterator<'a, T>
where
    T: TokenTrait,
{
    type Item = &'a [T];

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if let Some(sub_token) = self.tokens.get(self.cursor) {
                if sub_token.mask() != Mask::Continuation {
                    //return the previous buffer of subtokens (no copies!)
                    if self.cursor > self.begin {
                        let sub_tokens = &self.tokens[self.begin..self.cursor];
                        self.begin = self.cursor;
                        self.cursor += 1;
                        return Some(sub_tokens);
                    }
                }
                self.cursor += 1;
            } else {
                //we are at past the last item, return remaining buffer
                if self.begin < self.cursor {
                    let sub_tokens = &self.tokens[self.begin..self.cursor];
                    self.cursor += 1;
                    self.begin = self.cursor;
                    return Some(sub_tokens);
                } else {
                    //nothing in buffer, we're done
                    return None;
                }
            }
        }
    }
}

/// # ConsolidatableTokens
///
/// This trait can be implemented for collections of tokens (i.e. things that implement `TokenTrait`)
/// and instantiates an iterator to quickly iterate over the tokens in consolidated form, e.g.
/// grouping subtokens into words.
///
/// ```no_run
/// use rust_tokenizers::preprocessing::tokenizer::base_tokenizer::{Token, ConsolidatableTokens};
/// let tokens: Vec<Token> = vec!(); //add some tokens
/// for (wordcount, word_tokens) in tokens.iter_consolidate_tokens().enumerate() {
///       eprintln!("word #{} - {:?}", wordcount+1, word_tokens);
/// }
/// ```
pub trait ConsolidatableTokens<T>
where
    T: TokenTrait,
{
    fn iter_consolidate_tokens(&self) -> ConsolidatedTokenIterator<T>;
}

impl ConsolidatableTokens<Token> for Vec<Token> {
    fn iter_consolidate_tokens(&self) -> ConsolidatedTokenIterator<Token> {
        ConsolidatedTokenIterator::new(self)
    }
}

impl<'a> ConsolidatableTokens<TokenRef<'a>> for Vec<TokenRef<'a>> {
    fn iter_consolidate_tokens(&self) -> ConsolidatedTokenIterator<TokenRef<'a>> {
        ConsolidatedTokenIterator::new(self)
    }
}

#[derive(Debug, PartialEq, Clone)]
///A token that references the original text
///An owned token
pub struct Token {
    pub text: String,
    pub offset: Offset,
    pub reference_offsets: Vec<OffsetSize>,
    pub mask: Mask,
}

impl Token {
    pub fn new(text: String) -> Token {
        let text_size: OffsetSize = text.chars().count() as OffsetSize;
        Token {
            text,
            offset: Offset {
                begin: 0,
                end: text_size,
            },
            reference_offsets: (0..text_size).collect(),
            mask: Mask::None,
        }
    }

    pub fn as_ref(&self) -> TokenRef {
        //not a real implementation of AsRef because we do something slightly different
        TokenRef::from(self)
    }
}

impl Offset {
    pub fn new(begin: OffsetSize, end: OffsetSize) -> Offset {
        Offset { begin, end }
    }

    pub fn into_option(self) -> Option<Offset> {
        if self.end > self.begin {
            Some(self)
        } else {
            None
        }
    }
}

#[derive(Debug, PartialEq, PartialOrd, Clone)]
pub struct TokenizedInput {
    ///Vector of token IDs
    pub token_ids: Vec<i64>,

    ///Vector segments ids, segments are seperated with a [SEP] marker, each increments the segment ID. This vector has the same length as token_ids.
    pub segment_ids: Vec<i8>,

    ///Flags tokens as special tokens (1) or not (0). This vector has the same length as token_ids.
    pub special_tokens_mask: Vec<i8>,

    pub overflowing_tokens: Vec<i64>,
    pub num_truncated_tokens: usize,

    ///Offset information in relation to the original text. Tokens that can not be related to the
    ///original source are registered as None.
    pub token_offsets: Vec<Option<Offset>>,

    pub reference_offsets: Vec<Vec<OffsetSize>>,

    ///Masks tokens so you can see what type of token something is. This vector has the same length
    ///as token_ids (and also makes special_tokens_mask redundant).
    pub mask: Vec<Mask>,
}

pub trait Tokenizer<T: Vocab> {
    fn vocab(&self) -> &T;

    ///Tokenize a string, returns a vector of tokens as strings.
    ///Use `tokenize_with_offsets` or `tokenize_to_tokens` if you also want offset information.
    fn tokenize(&self, text: &str) -> Vec<String> {
        self.tokenize_with_offsets(text).0
    }

    ///Tokenize a string, return offset information
    fn tokenize_with_offsets(
        &self,
        text: &str,
    ) -> (
        Vec<String>,
        Vec<Option<Offset>>,
        Vec<Vec<OffsetSize>>,
        Vec<Mask>,
    ) {
        if text.trim().is_empty() {
            return (vec![], vec![], vec![], vec![]);
        }
        let initial_offsets = (0..text.chars().count() as OffsetSize).collect::<Vec<OffsetSize>>();
        let initial_token: TokenRef<'_> = TokenRef::new(text, &initial_offsets);
        let tokens = self.tokenize_to_tokens(initial_token);
        let length = tokens.len();
        let mut texts = Vec::with_capacity(length);
        let mut offsets = Vec::with_capacity(length);
        let mut original_positions = Vec::with_capacity(length);
        let mut masks = Vec::with_capacity(length);

        for token in tokens {
            texts.push(token.text);
            offsets.push(if !token.reference_offsets.is_empty() {
                Some(Offset {
                    begin: *token.reference_offsets.first().unwrap(),
                    end: *token.reference_offsets.last().unwrap() + 1,
                })
            } else {
                None
            });
            original_positions.push(token.reference_offsets);
            masks.push(token.mask);
        }
        (texts, offsets, original_positions, masks)
    }

    ///Tokenize a text, returns a vector of tokens (contains offset information and more)
    fn tokenize_to_tokens(&self, text: TokenRef) -> Vec<Token>;

    ///Tokenize a vector of strings, where each corresponds to for example a sentence, returns a vector of vectors of strings.
    ///Use `tokenize_list_with_offsets` if you also want offset information.
    fn tokenize_list(&self, text_list: Vec<&str>) -> Vec<Vec<String>> {
        text_list
            .into_iter()
            .map(|text| self.tokenize(text))
            .collect()
    }

    ///Tokenize a vector of strings, where each corresponds to for example a sentence, returns a vector of pairs consists of a vector of tokens and a list of offset information.
    fn tokenize_list_with_offsets(
        &self,
        text_list: Vec<&str>,
    ) -> Vec<(
        Vec<String>,
        Vec<Option<Offset>>,
        Vec<Vec<OffsetSize>>,
        Vec<Mask>,
    )> {
        text_list
            .into_iter()
            .map(|text| self.tokenize_with_offsets(text))
            .collect()
    }

    fn convert_tokens_to_ids(&self, tokens: &Vec<String>) -> Vec<i64> {
        tokens
            .into_iter()
            .map(|v| self.vocab().token_to_id(v))
            .collect()
    }

    fn encode(
        &self,
        text_1: &str,
        text_2: Option<&str>,
        max_len: usize,
        truncation_strategy: &TruncationStrategy,
        stride: usize,
    ) -> TokenizedInput {
        let (token_strings, token_offsets, original_positions, token_mask) =
            self.tokenize_with_offsets(text_1);
        let token_ids_1 = self.convert_tokens_to_ids(&token_strings);
        let len_1 = token_ids_1.len();
        let (token_ids_2, token_offsets_2, original_positions_2, token_mask_2, len_2, pair) = {
            if let Some(text) = text_2 {
                let (token_strings_2, token_offsets_2, original_positions_2, token_mask_2) =
                    self.tokenize_with_offsets(text);
                let token_ids_2: Vec<i64> = self.convert_tokens_to_ids(&token_strings_2);
                let len_2 = token_ids_2.len();
                (
                    Some(token_ids_2),
                    Some(token_offsets_2),
                    Some(original_positions_2),
                    Some(token_mask_2),
                    len_2,
                    Some(vec![]),
                )
            } else {
                (None, None, None, None, 0, None)
            }
        };
        let (additional_tokens, _, _, _, _additional_offsets, _additional_mask) = self
            .build_input_with_special_tokens(
                vec![],
                pair,
                vec![],
                Some(vec![]),
                vec![],
                Some(vec![]),
                vec![],
                Some(vec![]),
            );
        let total_len = len_1 + len_2 + additional_tokens.len();
        let num_truncated_tokens = if total_len > max_len {
            total_len - max_len
        } else {
            0
        };
        let (
            token_ids_1,
            token_ids_2,
            token_offsets,
            token_offsets_2,
            original_positions,
            original_positions_2,
            token_mask,
            token_mask_2,
            overflowing_tokens,
            _overflowing_offsets,
        ) = truncate_sequences(
            token_ids_1,
            token_ids_2,
            token_offsets,
            token_offsets_2,
            original_positions,
            original_positions_2,
            token_mask,
            token_mask_2,
            num_truncated_tokens,
            truncation_strategy,
            stride,
        )
        .unwrap();

        let (
            token_ids,
            segment_ids,
            special_tokens_mask,
            token_offsets,
            reference_offsets,
            token_mask,
        ) = self.build_input_with_special_tokens(
            token_ids_1,
            token_ids_2,
            token_offsets,
            token_offsets_2,
            original_positions,
            original_positions_2,
            token_mask,
            token_mask_2,
        );

        TokenizedInput {
            token_ids,
            segment_ids,
            special_tokens_mask,
            overflowing_tokens,
            num_truncated_tokens,
            token_offsets,
            reference_offsets,
            mask: token_mask,
        }
    }

    fn encode_list(
        &self,
        text_list: Vec<&str>,
        max_len: usize,
        truncation_strategy: &TruncationStrategy,
        stride: usize,
    ) -> Vec<TokenizedInput> {
        text_list
            .into_iter()
            .map(|text| self.encode(text, None, max_len, truncation_strategy, stride))
            .collect()
    }

    fn encode_pair_list(
        &self,
        text_list: Vec<(&str, &str)>,
        max_len: usize,
        truncation_strategy: &TruncationStrategy,
        stride: usize,
    ) -> Vec<TokenizedInput> {
        text_list
            .into_iter()
            .map(|text| self.encode(text.0, Some(text.1), max_len, truncation_strategy, stride))
            .collect()
    }

    fn decode_to_vec(&self, token_ids: Vec<i64>, skip_special_tokens: bool) -> Vec<String> {
        let tokens: Vec<String> = if skip_special_tokens {
            token_ids
                .iter()
                .filter(|id| !self.vocab().special_indices().contains_key(id))
                .map(|id| self.vocab().id_to_token(id))
                .collect_vec()
        } else {
            token_ids
                .iter()
                .map(|id| self.vocab().id_to_token(id))
                .collect_vec()
        };
        tokens
    }

    ///Converts a sequence of ids (integer) into  astring, using the tokenizer and vocabulary
    ///  with options to remove special tokens and clean up tokenization spaces.
    ///  Args:
    ///   * token_ids: list of tokenized input ids. Can be obtained using the `encode` or `encode_plus` methods.
    ///   * skip_special_tokens: if set to True, will replace special tokens.
    ///   * clean_up_tokenization_spaces: if set to True, will clean up the tokenization spaces.
    fn decode(
        &self,
        token_ids: Vec<i64>,
        skip_special_tokens: bool,
        clean_up_tokenization_spaces: bool,
    ) -> String {
        let tokens = self.decode_to_vec(token_ids, skip_special_tokens);
        let decoded_string = self.convert_tokens_to_string(tokens);
        if clean_up_tokenization_spaces {
            self.clean_up_tokenization(decoded_string)
        } else {
            decoded_string
        }
    }

    fn convert_tokens_to_string(&self, tokens: Vec<String>) -> String {
        tokens.join(" ")
    }

    fn clean_up_tokenization(&self, input_string: String) -> String {
        input_string
            .replace(" .", ".")
            .replace(" !", "!")
            .replace(" ?", "?")
            .replace(" ,", ",")
            .replace(" ' ", "'")
            .replace(" n't", "n't")
            .replace(" 'm", "'m")
            .replace(" do not", " don't")
            .replace(" 's", "'s")
            .replace(" 've", "'ve")
            .replace(" 're", "'re")
    }

    fn decode_list(
        &self,
        token_ids_list: Vec<Vec<i64>>,
        skip_special_tokens: bool,
        clean_up_tokenization_spaces: bool,
    ) -> Vec<String> {
        token_ids_list
            .into_iter()
            .map(|token_ids| {
                self.decode(token_ids, skip_special_tokens, clean_up_tokenization_spaces)
            })
            .collect()
    }

    /// Build model inputs from a sequence or a pair of sequence for sequence classification tasks
    /// by concatenating and adding special tokens.
    /// A RoBERTa sequence has the following format:
    /// single sequence: <s> X </s>
    /// pair of sequences: <s> A </s></s> B </s>
    ///
    /// Returns a tuple of:
    ///  * output token IDs
    ///  * token segment IDs
    ///  * special token mask
    ///  * offsets (as a vector of `Option<Offset>` because some added markers may not have associated offsets
    ///  * token mask
    fn build_input_with_special_tokens(
        &self,
        mut tokens_1: Vec<i64>,
        tokens_2: Option<Vec<i64>>,
        mut offsets_1: Vec<Option<Offset>>,
        offsets_2: Option<Vec<Option<Offset>>>,
        original_offsets_1: Vec<Vec<OffsetSize>>,
        original_offsets_2: Option<Vec<Vec<OffsetSize>>>,
        mut mask: Vec<Mask>,
        mask_2: Option<Vec<Mask>>,
    ) -> (
        Vec<i64>,
        Vec<i8>,
        Vec<i8>,
        Vec<Option<Offset>>,
        Vec<Vec<OffsetSize>>,
        Vec<Mask>,
    ) {
        let mut token_segment_ids: Vec<i8> = vec![0; tokens_1.len()];
        let mut special_tokens_mask: Vec<i8> = vec![0; tokens_1.len()];
        let mut original_offsets: Vec<Vec<OffsetSize>> = original_offsets_1;
        let output = match tokens_2 {
            Some(tokens) => {
                let length = tokens.len();
                token_segment_ids.extend(vec![1; length]);
                special_tokens_mask.extend(vec![0; length]);
                tokens_1.extend(tokens);
                if let Some(offsets_2) = offsets_2 {
                    offsets_1.extend(offsets_2);
                } else {
                    offsets_1.extend(vec![None; length]);
                }
                if let Some(original_offset_2) = original_offsets_2 {
                    original_offsets.extend(original_offset_2)
                }
                if let Some(mask_2) = mask_2 {
                    mask.extend(mask_2)
                } else {
                    mask.extend(vec![Mask::None; length]);
                }
                tokens_1
            }
            None => tokens_1,
        };
        (
            output,
            token_segment_ids,
            special_tokens_mask,
            offsets_1,
            original_offsets,
            mask,
        )
    }
}

pub trait MultiThreadedTokenizer<T: Vocab>
where
    Self: std::marker::Sync + Send + Tokenizer<T>,
{
    fn vocab(&self) -> &T {
        Tokenizer::<T>::vocab(self)
    }

    fn tokenize_list_with_offsets(
        &self,
        text_list: Vec<&str>,
    ) -> Vec<(
        Vec<String>,
        Vec<Option<Offset>>,
        Vec<Vec<OffsetSize>>,
        Vec<Mask>,
    )> {
        text_list
            .par_iter()
            .map(|text| self.tokenize_with_offsets(text))
            .collect()
    }

    fn tokenize_list(&self, text_list: Vec<&str>) -> Vec<Vec<String>> {
        text_list
            .par_iter()
            .map(|text| self.tokenize(text))
            .collect()
    }

    fn encode_list(
        &self,
        text_list: Vec<&str>,
        max_len: usize,
        truncation_strategy: &TruncationStrategy,
        stride: usize,
    ) -> Vec<TokenizedInput> {
        text_list
            .par_iter()
            .map(|text| self.encode(text, None, max_len, truncation_strategy, stride))
            .collect()
    }

    fn encode_pair_list(
        &self,
        text_list: Vec<(&str, &str)>,
        max_len: usize,
        truncation_strategy: &TruncationStrategy,
        stride: usize,
    ) -> Vec<TokenizedInput> {
        text_list
            .par_iter()
            .map(|text| self.encode(text.0, Some(text.1), max_len, truncation_strategy, stride))
            .collect()
    }

    fn decode_list(
        &self,
        token_ids_list: Vec<Vec<i64>>,
        skip_special_tokens: bool,
        clean_up_tokenization_spaces: bool,
    ) -> Vec<String> {
        token_ids_list
            .par_iter()
            .map(|token_ids| {
                self.decode(
                    token_ids.to_vec(),
                    skip_special_tokens,
                    clean_up_tokenization_spaces,
                )
            })
            .collect()
    }
}

#[derive(Debug)]
pub struct BaseTokenizer<T: Vocab> {
    vocab: Arc<T>,
    lower_case: bool,
    strip_accents: bool,
}

impl<T: Vocab + Sync + Send> BaseTokenizer<T> {
    pub fn from_file(
        path: &str,
        lower_case: bool,
        strip_accents: bool,
    ) -> Result<BaseTokenizer<T>, TokenizerError> {
        let vocab = T::from_file(path)?;
        Ok(BaseTokenizer {
            vocab: Arc::new(vocab),
            lower_case,
            strip_accents,
        })
    }

    pub fn from_existing_vocab(
        vocab: Arc<T>,
        lower_case: bool,
        strip_accents: bool,
    ) -> BaseTokenizer<T> {
        BaseTokenizer {
            vocab,
            lower_case,
            strip_accents,
        }
    }
}

impl<T: Vocab + Sync + Send> Tokenizer<T> for BaseTokenizer<T> {
    fn vocab(&self) -> &T {
        &self.vocab
    }

    fn tokenize_to_tokens(&self, initial_token: TokenRef) -> Vec<Token> {
        //split on whitespace
        let tokens: Vec<Token> = whitespace_tokenize(initial_token)
            .into_iter()
            .map(|token| {
                //split on special tokens
                split_on_special_tokens(token, self.vocab.as_ref())
            })
            .flatten()
            .map(|token| {
                //split on punctuation (with care for maintaining special values)
                split_on_punct(token)
            })
            .flatten()
            .map(|token| {
                //tokenize CJK characters so each character is one token
                tokenize_cjk_chars(token)
            })
            .flatten()
            .map(|token| {
                // v-- this is where the token gets owned, all steps above handle TokenRefs (dealing with &str)
                let mut token = Token {
                    text: token.text.to_string(),
                    offset: token.offset,
                    reference_offsets: token.reference_offsets.to_vec(),
                    mask: token.mask,
                };
                if token.mask != Mask::Special && token.mask != Mask::Unknown {
                    //apply the necessary transformations to the actual tokens (unless it's a special value)
                    if self.lower_case {
                        lowercase(&mut token);
                    }
                    if self.strip_accents {
                        strip_accents(&mut token);
                    }
                }
                token
            })
            .filter(|token| !token.text.is_empty())
            .collect();

        tokens
    }
}

impl<T: Vocab + Sync + Send> MultiThreadedTokenizer<T> for BaseTokenizer<T> {}

//==============================
// Unit tests
//==============================
#[cfg(test)]
mod tests {
    extern crate anyhow;

    use super::*;
    use crate::preprocessing::vocab::base_vocab::swap_key_values;
    use crate::BertVocab;
    use std::collections::HashMap;

    fn generate_test_vocab() -> BertVocab {
        let values: HashMap<String, i64> = [
            ("hello".to_owned(), 0),
            ("world".to_owned(), 1),
            ("[UNK]".to_owned(), 2),
            ("!".to_owned(), 3),
            ("[CLS]".to_owned(), 4),
            ("[SEP]".to_owned(), 5),
            ("[MASK]".to_owned(), 6),
            ("中".to_owned(), 7),
            ("华".to_owned(), 8),
            ("人".to_owned(), 9),
            ("[PAD]".to_owned(), 10),
            ("una".to_owned(), 11),
            ("##ffa".to_owned(), 12),
            ("##ble".to_owned(), 13),
        ]
        .iter()
        .cloned()
        .collect();

        let special_values: HashMap<String, i64> = [
            ("[UNK]".to_owned(), 2),
            ("[CLS]".to_owned(), 4),
            ("[SEP]".to_owned(), 5),
            ("[MASK]".to_owned(), 6),
            ("[PAD]".to_owned(), 10),
        ]
        .iter()
        .cloned()
        .collect();

        let indices = swap_key_values(&values);
        let special_indices = swap_key_values(&special_values);

        BertVocab {
            values,
            indices,
            unknown_value: "[UNK]",
            special_values,
            special_indices,
        }
    }

    #[test]
    fn test_base_tokenizer() -> anyhow::Result<()> {
        //        Given
        let vocab = Arc::new(generate_test_vocab());
        let base_tokenizer: BaseTokenizer<BertVocab> =
            BaseTokenizer::from_existing_vocab(vocab, true, true);
        let test_tuples = [
            (
                "Sentence with [MASK] token.",
                (
                    vec!["sentence", "with", "[MASK]", "token", "."],
                    vec![
                        Some(Offset::new(0, 8)),
                        Some(Offset::new(9, 13)),
                        Some(Offset::new(14, 20)),
                        Some(Offset::new(21, 26)),
                        Some(Offset::new(26, 27)),
                    ],
                    vec![
                        vec![0, 1, 2, 3, 4, 5, 6, 7],
                        vec![9, 10, 11, 12],
                        vec![14, 15, 16, 17, 18, 19],
                        vec![21, 22, 23, 24, 25],
                        vec![26],
                    ],
                    vec![
                        Mask::None,
                        Mask::None,
                        Mask::Special,
                        Mask::None,
                        Mask::Punctuation,
                    ],
                ),
            ),
            (
                "[CLS]",
                (
                    vec!["[CLS]"],
                    vec![Some(Offset::new(0, 5))],
                    vec![vec![0, 1, 2, 3, 4]],
                    vec![Mask::Special],
                ),
            ),
            (
                "[CLS] [PAD]",
                (
                    vec!["[CLS]", "[PAD]"],
                    vec![Some(Offset::new(0, 5)), Some(Offset::new(6, 11))],
                    vec![vec![0, 1, 2, 3, 4], vec![6, 7, 8, 9, 10]],
                    vec![Mask::Special, Mask::Special],
                ),
            ),
            (
                "[CLS]       [PAD]",
                (
                    vec!["[CLS]", "[PAD]"],
                    vec![Some(Offset::new(0, 5)), Some(Offset::new(12, 17))],
                    vec![vec![0, 1, 2, 3, 4], vec![12, 13, 14, 15, 16]],
                    vec![Mask::Special, Mask::Special],
                ),
            ),
            (
                "asdf",
                (
                    vec!["asdf"],
                    vec![Some(Offset::new(0, 4))],
                    vec![vec![0, 1, 2, 3]],
                    vec![Mask::None],
                ),
            ),
            ("", (vec![], vec![], vec![], vec![])),
            (
                "Allons, Flipote, allons; que d'eux je me délivre.",
                (
                    vec![
                        "allons", ",", "flipote", ",", "allons", ";", "que", "d", "\'", "eux",
                        "je", "me", "delivre", ".",
                    ],
                    vec![
                        Some(Offset { begin: 0, end: 6 }),
                        Some(Offset { begin: 6, end: 7 }),
                        Some(Offset { begin: 8, end: 15 }),
                        Some(Offset { begin: 15, end: 16 }),
                        Some(Offset { begin: 17, end: 23 }),
                        Some(Offset { begin: 23, end: 24 }),
                        Some(Offset { begin: 25, end: 28 }),
                        Some(Offset { begin: 29, end: 30 }),
                        Some(Offset { begin: 30, end: 31 }),
                        Some(Offset { begin: 31, end: 34 }),
                        Some(Offset { begin: 35, end: 37 }),
                        Some(Offset { begin: 38, end: 40 }),
                        Some(Offset { begin: 41, end: 48 }),
                        Some(Offset { begin: 48, end: 49 }),
                    ],
                    vec![
                        vec![0, 1, 2, 3, 4, 5],
                        vec![6],
                        vec![8, 9, 10, 11, 12, 13, 14],
                        vec![15],
                        vec![17, 18, 19, 20, 21, 22],
                        vec![23],
                        vec![25, 26, 27],
                        vec![29],
                        vec![30],
                        vec![31, 32, 33],
                        vec![35, 36],
                        vec![38, 39],
                        vec![41, 42, 43, 44, 45, 46, 47],
                        vec![48],
                    ],
                    vec![
                        Mask::None,
                        Mask::Punctuation,
                        Mask::None,
                        Mask::Punctuation,
                        Mask::None,
                        Mask::Punctuation,
                        Mask::None,
                        Mask::None,
                        Mask::Punctuation,
                        Mask::None,
                        Mask::None,
                        Mask::None,
                        Mask::None,
                        Mask::Punctuation,
                    ],
                ),
            ),
            (
                "[UNK]中华人民共和国 [PAD] asdf",
                (
                    vec![
                        "[UNK]", "中", "华", "人", "民", "共", "和", "国", "[PAD]", "asdf",
                    ],
                    vec![
                        Some(Offset { begin: 0, end: 5 }),
                        Some(Offset { begin: 5, end: 6 }),
                        Some(Offset { begin: 6, end: 7 }),
                        Some(Offset { begin: 7, end: 8 }),
                        Some(Offset { begin: 8, end: 9 }),
                        Some(Offset { begin: 9, end: 10 }),
                        Some(Offset { begin: 10, end: 11 }),
                        Some(Offset { begin: 11, end: 12 }),
                        Some(Offset { begin: 13, end: 18 }),
                        Some(Offset { begin: 19, end: 23 }),
                    ],
                    vec![
                        vec![0, 1, 2, 3, 4],
                        vec![5],
                        vec![6],
                        vec![7],
                        vec![8],
                        vec![9],
                        vec![10],
                        vec![11],
                        vec![13, 14, 15, 16, 17],
                        vec![19, 20, 21, 22],
                    ],
                    vec![
                        Mask::Unknown,
                        Mask::CJK,
                        Mask::CJK,
                        Mask::CJK,
                        Mask::CJK,
                        Mask::CJK,
                        Mask::CJK,
                        Mask::CJK,
                        Mask::Special,
                        Mask::None,
                    ],
                ),
            ),
        ];
        let source_texts: Vec<&str> = test_tuples.iter().map(|v| v.0).collect();

        //        When & Then
        for (source_text, expected_result) in test_tuples.iter() {
            let (tokens, offsets, offset_positions, mask) =
                base_tokenizer.tokenize_with_offsets(*source_text);
            let tokens: Vec<&str> = tokens.iter().map(|t| t.as_str()).collect();
            assert_eq!(tokens, expected_result.0);
            assert_eq!(offsets, expected_result.1);
            assert_eq!(offset_positions, expected_result.2);
            assert_eq!(mask, expected_result.3);
        }

        let results = Tokenizer::tokenize_list_with_offsets(&base_tokenizer, source_texts.clone());
        for ((_, expected_result), (tokens, offsets, offset_positions, mask)) in
            test_tuples.iter().zip(results.iter())
        {
            let tokens: Vec<&str> = tokens.iter().map(|t| t.as_str()).collect();
            assert_eq!(tokens, expected_result.0);
            assert_eq!(*offsets, expected_result.1);
            assert_eq!(*offset_positions, expected_result.2);
            assert_eq!(*mask, expected_result.3);
        }

        let results = MultiThreadedTokenizer::tokenize_list_with_offsets(
            &base_tokenizer,
            source_texts.clone(),
        );
        for ((_, expected_result), (tokens, offsets, offset_positions, mask)) in
            test_tuples.iter().zip(results.iter())
        {
            let tokens: Vec<&str> = tokens.iter().map(|t| t.as_str()).collect();
            assert_eq!(tokens, expected_result.0);
            assert_eq!(*offsets, expected_result.1);
            assert_eq!(*offset_positions, expected_result.2);
            assert_eq!(*mask, expected_result.3);
        }
        Ok(())
    }

    #[test]
    fn test_no_lower_casing() {
        //        Given
        let vocab = Arc::new(generate_test_vocab());
        let base_tokenizer: BaseTokenizer<BertVocab> =
            BaseTokenizer::from_existing_vocab(vocab, false, true);
        let test_tuples = [
            (
                "Sentence with [MASK] token.",
                (
                    vec!["Sentence", "with", "[MASK]", "token", "."],
                    vec![
                        Some(Offset::new(0, 8)),
                        Some(Offset::new(9, 13)),
                        Some(Offset::new(14, 20)),
                        Some(Offset::new(21, 26)),
                        Some(Offset::new(26, 27)),
                    ],
                    vec![
                        vec![0, 1, 2, 3, 4, 5, 6, 7],
                        vec![9, 10, 11, 12],
                        vec![14, 15, 16, 17, 18, 19],
                        vec![21, 22, 23, 24, 25],
                        vec![26],
                    ],
                    vec![
                        Mask::None,
                        Mask::None,
                        Mask::Special,
                        Mask::None,
                        Mask::Punctuation,
                    ],
                ),
            ),
            (
                "[CLS]",
                (
                    vec!["[CLS]"],
                    vec![Some(Offset::new(0, 5))],
                    vec![vec![0, 1, 2, 3, 4]],
                    vec![Mask::Special],
                ),
            ),
            (
                "[CLS] [PAD]",
                (
                    vec!["[CLS]", "[PAD]"],
                    vec![Some(Offset::new(0, 5)), Some(Offset::new(6, 11))],
                    vec![vec![0, 1, 2, 3, 4], vec![6, 7, 8, 9, 10]],
                    vec![Mask::Special, Mask::Special],
                ),
            ),
            (
                "[CLS]       [PAD]",
                (
                    vec!["[CLS]", "[PAD]"],
                    vec![Some(Offset::new(0, 5)), Some(Offset::new(12, 17))],
                    vec![vec![0, 1, 2, 3, 4], vec![12, 13, 14, 15, 16]],
                    vec![Mask::Special, Mask::Special],
                ),
            ),
            (
                "aSdF",
                (
                    vec!["aSdF"],
                    vec![Some(Offset::new(0, 4))],
                    vec![vec![0, 1, 2, 3]],
                    vec![Mask::None],
                ),
            ),
            ("", (vec![], vec![], vec![], vec![])),
            (
                "Allons, Flipote, allons; que d'eux je me délivre.",
                (
                    vec![
                        "Allons", ",", "Flipote", ",", "allons", ";", "que", "d", "\'", "eux",
                        "je", "me", "delivre", ".",
                    ],
                    vec![
                        Some(Offset { begin: 0, end: 6 }),
                        Some(Offset { begin: 6, end: 7 }),
                        Some(Offset { begin: 8, end: 15 }),
                        Some(Offset { begin: 15, end: 16 }),
                        Some(Offset { begin: 17, end: 23 }),
                        Some(Offset { begin: 23, end: 24 }),
                        Some(Offset { begin: 25, end: 28 }),
                        Some(Offset { begin: 29, end: 30 }),
                        Some(Offset { begin: 30, end: 31 }),
                        Some(Offset { begin: 31, end: 34 }),
                        Some(Offset { begin: 35, end: 37 }),
                        Some(Offset { begin: 38, end: 40 }),
                        Some(Offset { begin: 41, end: 48 }),
                        Some(Offset { begin: 48, end: 49 }),
                    ],
                    vec![
                        vec![0, 1, 2, 3, 4, 5],
                        vec![6],
                        vec![8, 9, 10, 11, 12, 13, 14],
                        vec![15],
                        vec![17, 18, 19, 20, 21, 22],
                        vec![23],
                        vec![25, 26, 27],
                        vec![29],
                        vec![30],
                        vec![31, 32, 33],
                        vec![35, 36],
                        vec![38, 39],
                        vec![41, 42, 43, 44, 45, 46, 47],
                        vec![48],
                    ],
                    vec![
                        Mask::None,
                        Mask::Punctuation,
                        Mask::None,
                        Mask::Punctuation,
                        Mask::None,
                        Mask::Punctuation,
                        Mask::None,
                        Mask::None,
                        Mask::Punctuation,
                        Mask::None,
                        Mask::None,
                        Mask::None,
                        Mask::None,
                        Mask::Punctuation,
                    ],
                ),
            ),
            (
                "[UNK]中华人民共和国 [PAD] asdf",
                (
                    vec![
                        "[UNK]", "中", "华", "人", "民", "共", "和", "国", "[PAD]", "asdf",
                    ],
                    vec![
                        Some(Offset { begin: 0, end: 5 }),
                        Some(Offset { begin: 5, end: 6 }),
                        Some(Offset { begin: 6, end: 7 }),
                        Some(Offset { begin: 7, end: 8 }),
                        Some(Offset { begin: 8, end: 9 }),
                        Some(Offset { begin: 9, end: 10 }),
                        Some(Offset { begin: 10, end: 11 }),
                        Some(Offset { begin: 11, end: 12 }),
                        Some(Offset { begin: 13, end: 18 }),
                        Some(Offset { begin: 19, end: 23 }),
                    ],
                    vec![
                        vec![0, 1, 2, 3, 4],
                        vec![5],
                        vec![6],
                        vec![7],
                        vec![8],
                        vec![9],
                        vec![10],
                        vec![11],
                        vec![13, 14, 15, 16, 17],
                        vec![19, 20, 21, 22],
                    ],
                    vec![
                        Mask::Unknown,
                        Mask::CJK,
                        Mask::CJK,
                        Mask::CJK,
                        Mask::CJK,
                        Mask::CJK,
                        Mask::CJK,
                        Mask::CJK,
                        Mask::Special,
                        Mask::None,
                    ],
                ),
            ),
        ];
        let source_texts: Vec<&str> = test_tuples.iter().map(|v| v.0).collect();

        //        When & Then
        for (source_text, expected_result) in test_tuples.iter() {
            let (tokens, offsets, offset_positions, mask) =
                base_tokenizer.tokenize_with_offsets(*source_text);
            let tokens: Vec<&str> = tokens.iter().map(|t| t.as_str()).collect();
            assert_eq!(tokens, expected_result.0);
            assert_eq!(offsets, expected_result.1);
            assert_eq!(offset_positions, expected_result.2);
            assert_eq!(mask, expected_result.3);
        }

        let results = Tokenizer::tokenize_list_with_offsets(&base_tokenizer, source_texts.clone());
        for ((_, expected_result), (tokens, offsets, offset_positions, mask)) in
            test_tuples.iter().zip(results.iter())
        {
            let tokens: Vec<&str> = tokens.iter().map(|t| t.as_str()).collect();
            assert_eq!(tokens, expected_result.0);
            assert_eq!(*offsets, expected_result.1);
            assert_eq!(*offset_positions, expected_result.2);
            assert_eq!(*mask, expected_result.3);
        }

        let results = MultiThreadedTokenizer::tokenize_list_with_offsets(
            &base_tokenizer,
            source_texts.clone(),
        );
        for ((_, expected_result), (tokens, offsets, offset_positions, mask)) in
            test_tuples.iter().zip(results.iter())
        {
            let tokens: Vec<&str> = tokens.iter().map(|t| t.as_str()).collect();
            assert_eq!(tokens, expected_result.0);
            assert_eq!(*offsets, expected_result.1);
            assert_eq!(*offset_positions, expected_result.2);
            assert_eq!(*mask, expected_result.3);
        }
    }

    #[test]
    fn test_convert_tokens_to_ids() {
        //        Given
        let vocab = Arc::new(generate_test_vocab());
        let base_tokenizer: BaseTokenizer<BertVocab> =
            BaseTokenizer::from_existing_vocab(vocab, true, true);
        let test_tuples = [
            (vec!["hello", "[MASK]", "world", "!"], vec![0, 6, 1, 3]),
            (
                vec!["hello", ",", "una", "##ffa", "##ble", "world", "!"],
                vec![0, 2, 11, 12, 13, 1, 3],
            ),
            (
                vec![
                    "[UNK]", "[UNK]", "华", "[UNK]", "[UNK]", "[UNK]", "[UNK]", "[UNK]", "[PAD]",
                    "[UNK]",
                ],
                vec![2, 2, 8, 2, 2, 2, 2, 2, 10, 2],
            ),
        ];

        //        When & Then
        for (source_text, expected_result) in test_tuples.iter() {
            assert_eq!(
                base_tokenizer.convert_tokens_to_ids(
                    source_text
                        .iter()
                        .map(|v| String::from(*v))
                        .collect::<Vec<_>>()
                        .as_ref()
                ),
                *expected_result
            );
        }
    }

    #[test]
    fn test_encode_single_sentence() {
        //        Given
        let vocab = Arc::new(generate_test_vocab());
        let base_tokenizer: BaseTokenizer<BertVocab> =
            BaseTokenizer::from_existing_vocab(vocab, true, true);
        let truncation_strategy = TruncationStrategy::LongestFirst;
        let test_tuples = [
            (
                "hello world!",
                TokenizedInput {
                    token_ids: vec![0, 1, 3],
                    segment_ids: vec![0, 0, 0],
                    special_tokens_mask: vec![0, 0, 0],
                    overflowing_tokens: vec![],
                    num_truncated_tokens: 0,
                    token_offsets: vec![
                        Some(Offset::new(0, 5)),
                        Some(Offset::new(6, 11)),
                        Some(Offset::new(11, 12)),
                    ],
                    reference_offsets: vec![vec![0, 1, 2, 3, 4], vec![6, 7, 8, 9, 10], vec![11]],
                    mask: vec![Mask::None, Mask::None, Mask::Punctuation],
                },
            ),
            (
                "hello, unaffable world!",
                TokenizedInput {
                    token_ids: vec![0, 2, 2, 1, 3],
                    segment_ids: vec![0, 0, 0, 0, 0],
                    special_tokens_mask: vec![0, 0, 0, 0, 0],
                    overflowing_tokens: vec![],
                    num_truncated_tokens: 0,
                    token_offsets: vec![
                        Some(Offset::new(0, 5)),
                        Some(Offset::new(5, 6)),
                        Some(Offset::new(7, 16)),
                        Some(Offset::new(17, 22)),
                        Some(Offset::new(22, 23)),
                    ],
                    reference_offsets: vec![
                        vec![0, 1, 2, 3, 4],
                        vec![5],
                        vec![7, 8, 9, 10, 11, 12, 13, 14, 15],
                        vec![17, 18, 19, 20, 21],
                        vec![22],
                    ],
                    mask: vec![
                        Mask::None,
                        Mask::Punctuation,
                        Mask::None,
                        Mask::None,
                        Mask::Punctuation,
                    ],
                },
            ),
            (
                "[UNK]中华人民共和国 [PAD] asdf",
                TokenizedInput {
                    token_ids: vec![2, 7, 8, 9, 2, 2, 2, 2, 10, 2],
                    segment_ids: vec![0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
                    special_tokens_mask: vec![0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
                    overflowing_tokens: vec![],
                    num_truncated_tokens: 0,
                    token_offsets: vec![
                        Some(Offset { begin: 0, end: 5 }),
                        Some(Offset { begin: 5, end: 6 }),
                        Some(Offset { begin: 6, end: 7 }),
                        Some(Offset { begin: 7, end: 8 }),
                        Some(Offset { begin: 8, end: 9 }),
                        Some(Offset { begin: 9, end: 10 }),
                        Some(Offset { begin: 10, end: 11 }),
                        Some(Offset { begin: 11, end: 12 }),
                        Some(Offset { begin: 13, end: 18 }),
                        Some(Offset { begin: 19, end: 23 }),
                    ],
                    reference_offsets: vec![
                        vec![0, 1, 2, 3, 4],
                        vec![5],
                        vec![6],
                        vec![7],
                        vec![8],
                        vec![9],
                        vec![10],
                        vec![11],
                        vec![13, 14, 15, 16, 17],
                        vec![19, 20, 21, 22],
                    ],
                    mask: vec![
                        Mask::Unknown,
                        Mask::CJK,
                        Mask::CJK,
                        Mask::CJK,
                        Mask::CJK,
                        Mask::CJK,
                        Mask::CJK,
                        Mask::CJK,
                        Mask::Special,
                        Mask::None,
                    ],
                },
            ),
            (
                "[UNK] a ! c ! e ! g ! i ! [PAD] a ! c ! e ! g ! i !",
                TokenizedInput {
                    token_ids: vec![2, 2, 3, 2, 3, 2, 3, 2, 3, 2],
                    segment_ids: vec![0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
                    special_tokens_mask: vec![0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
                    overflowing_tokens: vec![3, 10, 2, 3, 2, 3, 2, 3, 2, 3, 2, 3],
                    num_truncated_tokens: 12,
                    token_offsets: vec![
                        Some(Offset { begin: 0, end: 5 }),
                        Some(Offset { begin: 6, end: 7 }),
                        Some(Offset { begin: 8, end: 9 }),
                        Some(Offset { begin: 10, end: 11 }),
                        Some(Offset { begin: 12, end: 13 }),
                        Some(Offset { begin: 14, end: 15 }),
                        Some(Offset { begin: 16, end: 17 }),
                        Some(Offset { begin: 18, end: 19 }),
                        Some(Offset { begin: 20, end: 21 }),
                        Some(Offset { begin: 22, end: 23 }),
                    ],
                    reference_offsets: vec![
                        vec![0, 1, 2, 3, 4],
                        vec![6],
                        vec![8],
                        vec![10],
                        vec![12],
                        vec![14],
                        vec![16],
                        vec![18],
                        vec![20],
                        vec![22],
                    ],
                    mask: vec![
                        Mask::Unknown,
                        Mask::None,
                        Mask::Punctuation,
                        Mask::None,
                        Mask::Punctuation,
                        Mask::None,
                        Mask::Punctuation,
                        Mask::None,
                        Mask::Punctuation,
                        Mask::None,
                    ],
                },
            ),
        ];
        let source_texts: Vec<&str> = test_tuples.iter().map(|v| v.0).collect();
        let expected_results: Vec<TokenizedInput> =
            test_tuples.iter().map(|v| v.1.clone()).collect();

        //        When & Then
        for (source_text, expected_result) in test_tuples.iter() {
            let tokenized_input =
                base_tokenizer.encode(source_text, None, 10, &truncation_strategy, 0);
            assert_eq!(
                tokenized_input.token_ids.len(),
                tokenized_input.token_offsets.len(),
                "Offsets and tokens must have same length"
            );
            assert_eq!(tokenized_input, *expected_result, "Testing results");
        }
        assert_eq!(
            Tokenizer::encode_list(
                &base_tokenizer,
                source_texts.clone(),
                10,
                &truncation_strategy,
                0
            ),
            expected_results
        );
        assert_eq!(
            MultiThreadedTokenizer::encode_list(
                &base_tokenizer,
                source_texts.clone(),
                10,
                &truncation_strategy,
                0
            ),
            expected_results
        );
    }

    #[test]
    fn test_encode_sentence_pair() {
        //        Given
        let vocab = Arc::new(generate_test_vocab());
        let base_tokenizer: BaseTokenizer<BertVocab> =
            BaseTokenizer::from_existing_vocab(vocab, true, true);
        let truncation_strategy = TruncationStrategy::LongestFirst;
        let test_tuples = [
//            No truncation required
            (
                ("hello world!", "This is the second sentence"),
                TokenizedInput {
                    token_ids: vec!(0, 1, 3, 2, 2, 2, 2, 2),
                    segment_ids: vec!(0, 0, 0, 1, 1, 1, 1, 1),
                    special_tokens_mask: vec!(0, 0, 0, 0, 0, 0, 0, 0),
                    overflowing_tokens: vec!(),
                    num_truncated_tokens: 0,
                    token_offsets: vec!(Some(Offset::new(0, 5)), Some(Offset::new(6, 11)), Some(Offset::new(11, 12)), Some(Offset::new(0, 4)), Some(Offset::new(5, 7)), Some(Offset::new(8, 11)), Some(Offset::new(12, 18)), Some(Offset::new(19, 27))),
                    reference_offsets: vec!(vec!(0, 1, 2, 3, 4), vec!(6, 7, 8, 9, 10), vec!(11), vec!(0, 1, 2, 3), vec!(5, 6), vec!(8, 9, 10), vec!(12, 13, 14, 15, 16, 17), vec!(19, 20, 21, 22, 23, 24, 25, 26)),
                    mask: vec!(Mask::None, Mask::None, Mask::Punctuation, Mask::None, Mask::None, Mask::None, Mask::None, Mask::None),
                }
            ),
//            Truncation of sentence 2 (longest)
            (
                ("hello world!", "!This is the second sentence!!!"),
                TokenizedInput {
                    token_ids: vec!(0, 1, 3, 3, 2, 2, 2, 2, 2, 3),
                    segment_ids: vec!(0, 0, 0, 1, 1, 1, 1, 1, 1, 1),
                    special_tokens_mask: vec!(0, 0, 0, 0, 0, 0, 0, 0, 0, 0),
                    overflowing_tokens: vec!(),
                    num_truncated_tokens: 2,
                    token_offsets: vec!(
                        Some(Offset { begin: 0, end: 5 }), Some(Offset { begin: 6, end: 11 }), Some(Offset { begin: 11, end: 12 }), Some(Offset { begin: 0, end: 1 }), Some(Offset { begin: 1, end: 5 }), Some(Offset { begin: 6, end: 8 }), Some(Offset { begin: 9, end: 12 }), Some(Offset { begin: 13, end: 19 }), Some(Offset { begin: 20, end: 28 }), Some(Offset { begin: 28, end: 29 })
                    ),
                    reference_offsets: vec!(vec!(0, 1, 2, 3, 4), vec!(6, 7, 8, 9, 10), vec!(11), vec!(0), vec!(1, 2, 3, 4), vec!(6, 7), vec!(9, 10, 11), vec!(13, 14, 15, 16, 17, 18), vec!(20, 21, 22, 23, 24, 25, 26, 27), vec!(28)),
                    mask: vec!(Mask::None, Mask::None, Mask::Punctuation, Mask::Punctuation, Mask::None, Mask::None, Mask::None, Mask::None, Mask::None, Mask::Punctuation),
                }
            ),
//            Truncation of sentence 1 (longest)
            (
                ("[UNK] hello  hello  hello  hello  hello  hello  hello  hello  hello  hello  hello", "!!!"),
                TokenizedInput {
                    token_ids: vec!(2, 0, 0, 0, 0, 0, 0, 3, 3, 3),
                    segment_ids: vec!(0, 0, 0, 0, 0, 0, 0, 1, 1, 1),
                    special_tokens_mask: vec!(0, 0, 0, 0, 0, 0, 0, 0, 0, 0),
                    overflowing_tokens: vec!(0, 0, 0, 0, 0),
                    num_truncated_tokens: 5,
                    token_offsets: vec!(
                        Some(Offset { begin: 0, end: 5 }), Some(Offset { begin: 6, end: 11 }), Some(Offset { begin: 13, end: 18 }), Some(Offset { begin: 20, end: 25 }), Some(Offset { begin: 27, end: 32 }), Some(Offset { begin: 34, end: 39 }), Some(Offset { begin: 41, end: 46 }), Some(Offset { begin: 0, end: 1 }), Some(Offset { begin: 1, end: 2 }), Some(Offset { begin: 2, end: 3 })
                    ),
                    reference_offsets: vec!(vec!(0, 1, 2, 3, 4), vec!(6, 7, 8, 9, 10), vec!(13, 14, 15, 16, 17), vec!(20, 21, 22, 23, 24), vec!(27, 28, 29, 30, 31), vec!(34, 35, 36, 37, 38), vec!(41, 42, 43, 44, 45), vec!(0), vec!(1), vec!(2)),
                    mask: vec!(Mask::Unknown, Mask::None, Mask::None, Mask::None, Mask::None, Mask::None, Mask::None, Mask::Punctuation, Mask::Punctuation, Mask::Punctuation),
                }
            ),
//            Truncation of both sentences (longest)
            (
                ("[UNK] hello  hello  hello  hello  hello", "!!!!!!!!"),
                TokenizedInput {
                    token_ids: vec!(2, 0, 0, 0, 0, 3, 3, 3, 3, 3),
                    segment_ids: vec!(0, 0, 0, 0, 0, 1, 1, 1, 1, 1),
                    special_tokens_mask: vec!(0, 0, 0, 0, 0, 0, 0, 0, 0, 0),
                    overflowing_tokens: vec!(0),
                    num_truncated_tokens: 4,
                    token_offsets: vec!(
                        Some(Offset { begin: 0, end: 5 }), Some(Offset { begin: 6, end: 11 }), Some(Offset { begin: 13, end: 18 }), Some(Offset { begin: 20, end: 25 }), Some(Offset { begin: 27, end: 32 }), Some(Offset { begin: 0, end: 1 }), Some(Offset { begin: 1, end: 2 }), Some(Offset { begin: 2, end: 3 }), Some(Offset { begin: 3, end: 4 }), Some(Offset { begin: 4, end: 5 })
                    ),
                    reference_offsets: vec!(vec!(0, 1, 2, 3, 4), vec!(6, 7, 8, 9, 10), vec!(13, 14, 15, 16, 17), vec!(20, 21, 22, 23, 24), vec!(27, 28, 29, 30, 31), vec!(0), vec!(1), vec!(2), vec!(3), vec!(4)),
                    mask: vec!(Mask::Unknown, Mask::None, Mask::None, Mask::None, Mask::None, Mask::Punctuation, Mask::Punctuation, Mask::Punctuation, Mask::Punctuation, Mask::Punctuation),
                }
            )
        ];
        let source_texts: Vec<(&str, &str)> = test_tuples.iter().map(|v| v.0).collect();
        let expected_results: Vec<TokenizedInput> =
            test_tuples.iter().map(|v| v.1.clone()).collect();

        //        When & Then
        for (source_text, expected_result) in test_tuples.iter() {
            let tokenized_input = base_tokenizer.encode(
                source_text.0,
                Some(source_text.1),
                10,
                &truncation_strategy,
                0,
            );
            assert_eq!(
                tokenized_input.token_ids.len(),
                tokenized_input.token_offsets.len(),
                "Offsets and tokens must have same length"
            );
            assert_eq!(tokenized_input, *expected_result, "Testing results");
        }
        assert_eq!(
            Tokenizer::encode_pair_list(
                &base_tokenizer,
                source_texts.clone(),
                10,
                &truncation_strategy,
                0
            ),
            expected_results
        );
        assert_eq!(
            MultiThreadedTokenizer::encode_pair_list(
                &base_tokenizer,
                source_texts.clone(),
                10,
                &truncation_strategy,
                0
            ),
            expected_results
        );
    }

    #[test]
    fn test_decode() {
        //        Given
        let vocab = Arc::new(generate_test_vocab());
        let base_tokenizer: BaseTokenizer<BertVocab> =
            BaseTokenizer::from_existing_vocab(vocab, true, true);
        let skip_special_tokens = false;
        let clean_up_tokenization_spaces = false;
        let test_tuples = [
            (vec![0, 1, 3], "hello world !"),
            (vec![10, 0, 1, 3], "[PAD] hello world !"),
            (vec![10, 0, 1, 2, 3], "[PAD] hello world [UNK] !"),
        ];
        let source_ids: Vec<Vec<i64>> = test_tuples.iter().map(|v| v.0.clone()).collect_vec();
        let expected_results: Vec<&str> = test_tuples.iter().map(|v| v.1.clone()).collect_vec();

        //        When & Then
        for (source_ids, expected_result) in test_tuples.iter() {
            assert_eq!(
                base_tokenizer.decode(
                    source_ids.clone(),
                    skip_special_tokens,
                    clean_up_tokenization_spaces
                ),
                *expected_result
            );
        }
        assert_eq!(
            Tokenizer::decode_list(
                &base_tokenizer,
                source_ids.clone(),
                skip_special_tokens,
                clean_up_tokenization_spaces
            ),
            expected_results
        );
        assert_eq!(
            MultiThreadedTokenizer::decode_list(
                &base_tokenizer,
                source_ids.clone(),
                skip_special_tokens,
                clean_up_tokenization_spaces
            ),
            expected_results
        );
    }

    #[test]
    fn test_decode_skip_special_tokens() {
        //        Given
        let vocab = Arc::new(generate_test_vocab());
        let base_tokenizer: BaseTokenizer<BertVocab> =
            BaseTokenizer::from_existing_vocab(vocab, true, true);
        let skip_special_tokens = true;
        let clean_up_tokenization_spaces = false;
        let test_tuples = [
            (vec![0, 1, 3], "hello world !"),
            (vec![10, 0, 1, 3], "hello world !"),
            (vec![10, 0, 1, 2, 3], "hello world !"),
        ];
        let source_ids: Vec<Vec<i64>> = test_tuples.iter().map(|v| v.0.clone()).collect_vec();
        let expected_results: Vec<&str> = test_tuples.iter().map(|v| v.1.clone()).collect_vec();

        //        When & Then
        for (source_ids, expected_result) in test_tuples.iter() {
            assert_eq!(
                base_tokenizer.decode(
                    source_ids.clone(),
                    skip_special_tokens,
                    clean_up_tokenization_spaces
                ),
                *expected_result
            );
        }
        assert_eq!(
            Tokenizer::decode_list(
                &base_tokenizer,
                source_ids.clone(),
                skip_special_tokens,
                clean_up_tokenization_spaces
            ),
            expected_results
        );
        assert_eq!(
            MultiThreadedTokenizer::decode_list(
                &base_tokenizer,
                source_ids.clone(),
                skip_special_tokens,
                clean_up_tokenization_spaces
            ),
            expected_results
        );
    }

    #[test]
    fn test_decode_clean_up_tokenization_spaces() {
        //        Given
        let vocab = Arc::new(generate_test_vocab());
        let base_tokenizer: BaseTokenizer<BertVocab> =
            BaseTokenizer::from_existing_vocab(vocab, true, true);
        let skip_special_tokens = true;
        let clean_up_tokenization_spaces = true;
        let test_tuples = [
            (vec![0, 1, 3], "hello world!"),
            (vec![10, 0, 1, 3], "hello world!"),
            (vec![10, 0, 1, 2, 3], "hello world!"),
        ];
        let source_ids: Vec<Vec<i64>> = test_tuples.iter().map(|v| v.0.clone()).collect_vec();
        let expected_results: Vec<&str> = test_tuples.iter().map(|v| v.1.clone()).collect_vec();

        //        When & Then
        for (source_ids, expected_result) in test_tuples.iter() {
            assert_eq!(
                base_tokenizer.decode(
                    source_ids.clone(),
                    skip_special_tokens,
                    clean_up_tokenization_spaces
                ),
                *expected_result
            );
        }
        assert_eq!(
            Tokenizer::decode_list(
                &base_tokenizer,
                source_ids.clone(),
                skip_special_tokens,
                clean_up_tokenization_spaces
            ),
            expected_results
        );
        assert_eq!(
            MultiThreadedTokenizer::decode_list(
                &base_tokenizer,
                source_ids.clone(),
                skip_special_tokens,
                clean_up_tokenization_spaces
            ),
            expected_results
        );
    }

    #[test]
    fn test_consolidated_token_iterator() {
        let tokens = vec![
            Token {
                text: "he".to_owned(),
                offset: Offset::new(0, 2),
                reference_offsets: vec![0, 1],
                mask: Mask::Begin,
            },
            Token {
                text: "llo".to_owned(),
                offset: Offset::new(2, 5),
                reference_offsets: vec![2, 3, 4],
                mask: Mask::Continuation,
            },
            Token {
                text: "world".to_owned(),
                offset: Offset::new(6, 11),
                reference_offsets: vec![6, 7, 8, 9, 10],
                mask: Mask::None,
            },
            Token {
                text: "!".to_owned(),
                offset: Offset::new(11, 12),
                reference_offsets: vec![11],
                mask: Mask::Punctuation,
            },
        ];

        let mut iter = tokens.iter_consolidate_tokens();
        assert_eq!(iter.next(), Some(&tokens[0..2]));
        assert_eq!(iter.next(), Some(&tokens[2..3]));
        assert_eq!(iter.next(), Some(&tokens[3..4]));
        assert_eq!(iter.next(), None);
        assert_eq!(iter.next(), None); //calling it more times after ending should always keep returning None
    }
}
