// Copyright 2021 The Eleuther AI and HuggingFace Inc. team. All rights reserved.
// Copyright 2021 Guillaume Becquin
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//     http://www.apache.org/licenses/LICENSE-2.0
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use crate::common::dropout::Dropout;
use crate::gpt_neo::attention::{GptNeoAttention, GptNeoAttentionUtils};
use crate::gpt_neo::decoder::GptNeoBlock;
use crate::gpt_neo::LayerState;
use crate::pipelines::common::{ModelType, TokenizerOption};
use crate::pipelines::generation_utils::private_generation_utils::{
    PreparedInput, PrivateLanguageGenerator,
};
use crate::pipelines::generation_utils::{
    Cache, GenerateConfig, LMHeadModel, LMModelOutput, LanguageGenerator,
};
use crate::{Activation, Config, RustBertError};
use rust_tokenizers::tokenizer::Gpt2Tokenizer;
use rust_tokenizers::vocab::Gpt2Vocab;
use serde::{Deserialize, Serialize};
use std::borrow::{Borrow, BorrowMut};
use tch::{nn, Kind, Tensor};

/// # GPT-Neo Pretrained model weight files
pub struct GptNeoModelResources;

/// # GPT-Neo Pretrained model config files
pub struct GptNeoConfigResources;

/// # GPT-Neo Pretrained model vocab files
pub struct GptNeoVocabResources;

/// # GPT-Neo Pretrained model merges files
pub struct GptNeoMergesResources;

impl GptNeoModelResources {
    /// Shared under Apache 2.0 license by the EleutherAI contributors at https://www.eleuther.ai. Modified with conversion to C-array format.
    pub const GPT_NEO_125M: (&'static str, &'static str) = (
        "gpt-neo-125M/model",
        "https://huggingface.co/EleutherAI/gpt-neo-125M/resolve/main/rust_model.ot",
    );
}

impl GptNeoConfigResources {
    /// Shared under Apache 2.0 license by the EleutherAI contributors at https://www.eleuther.ai. Modified with conversion to C-array format.
    pub const GPT_NEO_125M: (&'static str, &'static str) = (
        "gpt-neo-125M/config",
        "https://huggingface.co/EleutherAI/gpt-neo-125M/resolve/main/config.json",
    );
}

impl GptNeoVocabResources {
    /// Shared under Modified MIT license by the OpenAI team at https://github.com/openai/gpt-2/blob/master/LICENSE. Modified with conversion to C-array format.
    pub const GPT_NEO_125M: (&'static str, &'static str) = (
        "gpt-neo-125M/vocab",
        "https://huggingface.co/gpt2/resolve/main/vocab.json",
    );
}

impl GptNeoMergesResources {
    /// Shared under Modified MIT license by the OpenAI team at https://github.com/openai/gpt-2/blob/master/LICENSE. Modified with conversion to C-array format.
    pub const GPT_NEO_125M: (&'static str, &'static str) = (
        "gpt-neo-125M/merges",
        "https://huggingface.co/gpt2/resolve/main/merges.txt",
    );
}

#[derive(Debug, Serialize, Deserialize, Clone, Copy)]
#[serde(rename_all = "camelCase")]
/// #GPT-Neo attention layer type
pub enum AttentionLayerType {
    Global,
    Local,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
/// # GPT-Neo model configuration
/// Defines the GPT-Neo model architecture (e.g. number of layers, hidden layer size, vocab size...).
pub struct GptNeoConfig {
    pub activation_function: Activation,
    pub attention_dropout: f64,
    pub attention_layers: Vec<AttentionLayerType>,
    pub attention_types: Vec<(Vec<AttentionLayerType>, i64)>,
    pub intermediate_size: Option<i64>,
    pub bos_token_id: i64,
    pub eos_token_id: i64,
    pub vocab_size: i64,
    pub num_layers: i64,
    pub num_heads: i64,
    pub hidden_size: i64,
    pub window_size: i64,
    pub embed_dropout: f64,
    pub initializer_range: f64,
    pub layer_norm_epsilon: f64,
    pub max_position_embeddings: i64,
    pub output_past: Option<bool>,
    pub output_attentions: Option<bool>,
    pub output_hidden_states: Option<bool>,
    pub resid_dropout: f64,
}

impl Config<GptNeoConfig> for GptNeoConfig {}

pub struct GptNeoModel {
    word_embeddings: nn::Embedding,
    position_embeddings: nn::Embedding,
    layers: Vec<GptNeoBlock>,
    dropout: Dropout,
    layer_norm: nn::LayerNorm,
    window_size: i64,
    output_attentions: bool,
    output_hidden_states: bool,
}

impl GptNeoAttentionUtils for GptNeoModel {}

impl GptNeoModel {
    pub fn new<'p, P>(p: P, config: &GptNeoConfig) -> Result<GptNeoModel, RustBertError>
    where
        P: Borrow<nn::Path<'p>>,
    {
        let p = p.borrow();

        let word_embeddings = nn::embedding(
            p / "wte",
            config.vocab_size,
            config.hidden_size,
            Default::default(),
        );

        let position_embeddings = nn::embedding(
            p / "wpe",
            config.max_position_embeddings,
            config.hidden_size,
            Default::default(),
        );

        let dropout = Dropout::new(config.embed_dropout);

        let layer_norm_config = nn::LayerNormConfig {
            eps: config.layer_norm_epsilon,
            ..Default::default()
        };

        let layer_norm = nn::layer_norm(p / "ln_f", vec![config.hidden_size], layer_norm_config);

        let mut layers: Vec<GptNeoBlock> = Vec::with_capacity(config.num_layers as usize);
        let p_layers = p / "h";
        for layer_index in 0..config.num_layers {
            layers.push(GptNeoBlock::new(
                &p_layers / layer_index,
                layer_index as usize,
                config,
            )?);
        }

        let window_size = config.window_size;

        let output_attentions = config.output_attentions.unwrap_or(false);
        let output_hidden_states = config.output_hidden_states.unwrap_or(false);

        Ok(GptNeoModel {
            word_embeddings,
            position_embeddings,
            layers,
            dropout,
            layer_norm,
            window_size,
            output_attentions,
            output_hidden_states,
        })
    }

    pub fn forward_t(
        &self,
        input_ids: Option<&Tensor>,
        input_embeds: Option<&Tensor>,
        token_type_ids: Option<&Tensor>,
        position_ids: Option<&Tensor>,
        layer_states: Option<Vec<Option<LayerState>>>,
        attention_mask: Option<&Tensor>,
        train: bool,
    ) -> Result<GptNeoModelOutput, RustBertError> {
        let (calc_input_embeddings, input_shape, device) = if let Some(input_ids) = input_ids {
            if input_embeds.is_none() {
                (
                    Some(input_ids.apply(&self.word_embeddings)),
                    input_ids.size(),
                    input_ids.device(),
                )
            } else {
                return Err(RustBertError::ValueError(
                    "Only one of input ids or input embeddings may be set".into(),
                ));
            }
        } else if let Some(input_embeds) = input_embeds {
            let mut input_shape = input_embeds.size();
            let _ = input_shape.pop();
            (None, input_shape, input_embeds.device())
        } else {
            return Err(RustBertError::ValueError(
                "At least one of input ids or input embeddings must be set".into(),
            ));
        };

        let (batch_size, current_sequence_length) = (input_shape[0], input_shape[1]);

        let past_length = if let Some(past_state_value) = &layer_states {
            if let Some(first_layer_state) = &past_state_value[0] {
                let mut size_iter = first_layer_state.prev_key.size().into_iter().rev();
                size_iter.next();
                size_iter.next().unwrap()
            } else {
                0
            }
        } else {
            0
        };

        let full_sequence_length = current_sequence_length + past_length;

        let calc_position_ids = if position_ids.is_none() {
            let position_ids =
                Tensor::arange1(past_length, full_sequence_length, (Kind::Int64, device));
            Some(
                position_ids
                    .unsqueeze(0)
                    .view([-1, current_sequence_length]),
            )
        } else {
            None
        };

        let position_ids = position_ids.unwrap_or_else(|| calc_position_ids.as_ref().unwrap());

        let global_attention_mask = attention_mask.map(|attention_mask_value| {
            let global_attention_mask = attention_mask_value
                .view([batch_size, -1])
                .unsqueeze(1)
                .unsqueeze(1);
            (1 - global_attention_mask) * -1e4
        });

        let local_attention_mask = GptNeoModel::create_local_attention_mask(
            batch_size,
            full_sequence_length,
            self.window_size,
            device,
            attention_mask,
        )?;

        let input_embeds = input_embeds.unwrap_or_else(|| calc_input_embeddings.as_ref().unwrap());
        let position_embeds = position_ids.apply(&self.position_embeddings);

        let mut hidden_state = input_embeds + position_embeds;
        if let Some(token_type_ids) = token_type_ids {
            hidden_state = hidden_state + token_type_ids.apply(&self.word_embeddings);
        };
        hidden_state = hidden_state.apply_t(&self.dropout, train);

        let mut output_shape = input_shape.clone();
        output_shape.push(*hidden_state.size().last().unwrap());

        let mut all_hidden_states: Option<Vec<Tensor>> = if self.output_hidden_states {
            Some(vec![])
        } else {
            None
        };
        let mut all_attentions: Option<Vec<Tensor>> = if self.output_attentions {
            Some(vec![])
        } else {
            None
        };
        let old_cache = layer_states.unwrap_or_else(|| vec![None; self.layers.len()]);
        let mut next_cache = vec![None; self.layers.len()];

        let mut x: Option<Tensor> = None;
        let mut attention_weights: Option<Tensor>;

        for ((layer_idx, layer), layer_state) in
            self.layers.iter().enumerate().zip(old_cache.into_iter())
        {
            let attention_mask = match layer.get_attention_type() {
                GptNeoAttention::SelfAttention(_) => global_attention_mask.as_ref(),
                GptNeoAttention::LocalSelfAttention(_) => Some(&local_attention_mask),
            };

            let temp = if let Some(x_value) = &x {
                if let Some(hidden_states) = all_hidden_states.borrow_mut() {
                    hidden_states.push(x_value.copy());
                }
                layer.forward_t(x_value, layer_state.as_ref(), attention_mask, train)?
            } else {
                if let Some(hidden_states) = all_hidden_states.borrow_mut() {
                    hidden_states.push(hidden_state.copy());
                }
                layer.forward_t(&hidden_state, layer_state.as_ref(), attention_mask, train)?
            };
            x = Some(temp.0);
            attention_weights = temp.1;
            next_cache[layer_idx] = temp.2;
            if let Some(attentions) = all_attentions.borrow_mut() {
                attentions.push(attention_weights.as_ref().unwrap().copy());
            };
        }
        if let Some(hidden_states) = all_hidden_states.borrow_mut() {
            hidden_states.push(x.as_ref().unwrap().copy());
        };

        let hidden_states = x
            .unwrap()
            .apply(&self.layer_norm)
            .view(output_shape.as_slice());

        Ok(GptNeoModelOutput {
            hidden_states,
            next_cache: Some(next_cache),
            all_hidden_states,
            all_attentions,
        })
    }
}

pub struct GptNeoForCausalLM {
    transformer: GptNeoModel,
}

impl GptNeoForCausalLM {
    pub fn new<'p, P>(p: P, config: &GptNeoConfig) -> Result<GptNeoForCausalLM, RustBertError>
    where
        P: Borrow<nn::Path<'p>>,
    {
        let p = p.borrow();

        let transformer = GptNeoModel::new(p / "transformer", config)?;

        Ok(GptNeoForCausalLM { transformer })
    }

    pub fn forward_t(
        &self,
        input_ids: Option<&Tensor>,
        input_embeds: Option<&Tensor>,
        token_type_ids: Option<&Tensor>,
        position_ids: Option<&Tensor>,
        layer_states: Option<Vec<Option<LayerState>>>,
        attention_mask: Option<&Tensor>,
        train: bool,
    ) -> Result<GptNeoModelLMOutput, RustBertError> {
        let base_model_output = self.transformer.forward_t(
            input_ids,
            input_embeds,
            token_type_ids,
            position_ids,
            layer_states,
            attention_mask,
            train,
        )?;

        let lm_logits = base_model_output
            .hidden_states
            .linear::<Tensor>(&self.transformer.word_embeddings.ws, None);

        Ok(GptNeoModelLMOutput {
            lm_logits,
            next_cache: base_model_output.next_cache,
            all_hidden_states: base_model_output.all_hidden_states,
            all_attentions: base_model_output.all_attentions,
        })
    }
}

impl LMHeadModel for GptNeoForCausalLM {
    fn forward_t(
        &self,
        input_ids: &Option<Tensor>,
        layer_past: Cache,
        attention_mask: &Option<Tensor>,
        token_type_ids: &Option<Tensor>,
        position_ids: &Option<Tensor>,
        input_embeds: &Option<Tensor>,
        _encoder_outputs: Option<&Tensor>,
        _decoder_input_ids: &Option<Tensor>,
        train: bool,
    ) -> Result<LMModelOutput, RustBertError> {
        let base_model_output = match layer_past {
            Cache::GPTNeoCache(layer_past) => self.forward_t(
                input_ids.as_ref(),
                input_embeds.as_ref(),
                token_type_ids.as_ref(),
                position_ids.as_ref(),
                layer_past,
                attention_mask.as_ref(),
                train,
            ),
            Cache::None => self.forward_t(
                input_ids.as_ref(),
                input_embeds.as_ref(),
                token_type_ids.as_ref(),
                position_ids.as_ref(),
                None,
                attention_mask.as_ref(),
                train,
            ),
            _ => {
                return Err(RustBertError::ValueError(
                    "Cache not compatible with GPT-Neo Model".into(),
                ));
            }
        }?;

        Ok(LMModelOutput {
            lm_logits: base_model_output.lm_logits,
            cache: Cache::GPTNeoCache(base_model_output.next_cache),
        })
    }
}

/// Container for the GPT-Neo model output.
pub struct GptNeoModelOutput {
    /// Last hidden states from the model
    pub hidden_states: Tensor,
    /// Cached outputs of the model (attention layers keys and values) if the model is used for generation
    pub next_cache: Option<Vec<Option<LayerState>>>,
    /// Hidden states for all intermediate layers
    pub all_hidden_states: Option<Vec<Tensor>>,
    /// Attention weights for all intermediate layers
    pub all_attentions: Option<Vec<Tensor>>,
}

///Container holding a GPT-Neo model with LM head output
pub struct GptNeoModelLMOutput {
    /// logits
    pub lm_logits: Tensor,
    /// Cached outputs of the model (attention layers keys and values) if the model is used for generation
    pub next_cache: Option<Vec<Option<LayerState>>>,
    /// Hidden states for all intermediate layers
    pub all_hidden_states: Option<Vec<Tensor>>,
    /// Attention weights for all intermediate layers
    pub all_attentions: Option<Vec<Tensor>>,
}

/// # Language generation model based on the GPT-Neo architecture
pub struct GptNeoGenerator {
    model: GptNeoForCausalLM,
    tokenizer: TokenizerOption,
    var_store: nn::VarStore,
    generate_config: GenerateConfig,
    bos_token_id: Option<i64>,
    eos_token_ids: Option<Vec<i64>>,
    pad_token_id: Option<i64>,
    is_encoder_decoder: bool,
    vocab_size: i64,
    decoder_start_id: Option<i64>,
}

impl GptNeoGenerator {
    /// Build a new `GPTNeoGenerator`
    ///
    /// # Arguments
    ///
    /// * `generate_config` - `GenerateConfig` object containing the resource references (model, vocabulary, configuration), generation options and device placement (CPU/GPU)
    ///
    /// # Example
    ///
    /// ```no_run
    /// # fn main() -> anyhow::Result<()> {
    /// use rust_bert::gpt_neo::GPTNeoGenerator;
    /// use rust_bert::pipelines::generation_utils::GenerateConfig;
    ///
    /// let generate_config = GenerateConfig {
    ///     max_length: 30,
    ///     do_sample: true,
    ///     num_beams: 5,
    ///     temperature: 1.1,
    ///     num_return_sequences: 3,
    ///     ..Default::default()
    /// };
    /// let gpt2_generator = GPTNeoGenerator::new(generate_config)?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn new(generate_config: GenerateConfig) -> Result<GptNeoGenerator, RustBertError> {
        let config_path = generate_config.config_resource.get_local_path()?;
        let vocab_path = generate_config.vocab_resource.get_local_path()?;
        let merges_path = generate_config.merges_resource.get_local_path()?;
        let weights_path = generate_config.model_resource.get_local_path()?;
        let device = generate_config.device;

        generate_config.validate();
        let mut var_store = nn::VarStore::new(device);
        let tokenizer = TokenizerOption::from_file(
            ModelType::GPTNeo,
            vocab_path.to_str().unwrap(),
            Some(merges_path.to_str().unwrap()),
            false,
            None,
            None,
        )?;
        let config = GptNeoConfig::from_file(config_path);
        let model = GptNeoForCausalLM::new(&var_store.root(), &config)?;
        var_store.load(weights_path)?;

        let bos_token_id = Some(tokenizer.convert_tokens_to_ids(&[Gpt2Vocab::bos_value()])[0]);
        let eos_token_ids = Some(tokenizer.convert_tokens_to_ids(&[Gpt2Vocab::eos_value()]));
        let pad_token_id = Some(tokenizer.convert_tokens_to_ids(&[Gpt2Vocab::eos_value()])[0]);
        let is_encoder_decoder = false;
        let vocab_size = config.vocab_size;
        let decoder_start_id = None;

        Ok(GptNeoGenerator {
            model,
            tokenizer,
            var_store,
            generate_config,
            bos_token_id,
            eos_token_ids,
            pad_token_id,
            is_encoder_decoder,
            vocab_size,
            decoder_start_id,
        })
    }
}

impl PrivateLanguageGenerator<GptNeoForCausalLM, Gpt2Vocab, Gpt2Tokenizer> for GptNeoGenerator {
    fn get_model(&self) -> &GptNeoForCausalLM {
        &self.model
    }
    fn get_tokenizer(&self) -> &TokenizerOption {
        &self.tokenizer
    }
    fn get_var_store(&self) -> &nn::VarStore {
        &self.var_store
    }
    fn get_config(&self) -> &GenerateConfig {
        &self.generate_config
    }
    fn get_bos_id(&self) -> &Option<i64> {
        &self.bos_token_id
    }
    fn get_eos_ids(&self) -> &Option<Vec<i64>> {
        &self.eos_token_ids
    }
    fn get_pad_id(&self) -> &Option<i64> {
        &self.pad_token_id
    }
    fn is_encoder_decoder(&self) -> bool {
        self.is_encoder_decoder
    }
    fn get_vocab_size(&self) -> i64 {
        self.vocab_size
    }
    fn get_decoder_start_id(&self) -> Option<i64> {
        self.decoder_start_id
    }

    fn prepare_inputs_for_generation<'a>(
        &self,
        input_ids: Tensor,
        _encoder_outputs: Option<&'a Tensor>,
        past: Cache,
        attention_mask: Tensor,
    ) -> PreparedInput<'a> {
        let position_ids = (attention_mask.totype(Kind::Int64).cumsum(-1, Kind::Int64) - 1)
            .masked_fill(&attention_mask.eq(0), 1);

        match past {
            Cache::GPTNeoCache(past) => {
                if past.is_some() {
                    PreparedInput {
                        prepared_input: Some(input_ids.select(1, -1).unsqueeze(-1)),
                        prepared_attention_mask: Some(attention_mask),
                        prepared_encoder_output: None,
                        prepared_decoder_input: None,
                        prepared_position_ids: Some(position_ids.select(1, -1).unsqueeze(-1)),
                        prepared_past: Cache::GPTNeoCache(past),
                    }
                } else {
                    PreparedInput {
                        prepared_input: Some(input_ids),
                        prepared_attention_mask: Some(attention_mask),
                        prepared_encoder_output: None,
                        prepared_decoder_input: None,
                        prepared_position_ids: Some(position_ids),
                        prepared_past: Cache::GPTNeoCache(None),
                    }
                }
            }
            Cache::None => PreparedInput {
                prepared_input: Some(input_ids),
                prepared_attention_mask: Some(attention_mask),
                prepared_encoder_output: None,
                prepared_decoder_input: None,
                prepared_position_ids: Some(position_ids),
                prepared_past: Cache::GPTNeoCache(None),
            },
            _ => panic!("Cache type incompatible with GPT-Neo"),
        }
    }

    fn reorder_cache(
        &self,
        past: &mut Cache,
        _encoder_outputs: Option<Tensor>,
        beam_indices: &Tensor,
    ) -> Option<Tensor> {
        match past {
            Cache::GPTNeoCache(cached_decoder_state) => match cached_decoder_state {
                Some(old_cache) => {
                    for layer_state in old_cache.iter_mut() {
                        if layer_state.is_some() {
                            layer_state.as_mut().unwrap().reorder_cache(beam_indices)
                        };
                    }
                    None
                }
                None => None,
            },
            Cache::None => None,
            _ => {
                panic!("Invalid cache for GPT-Neo model");
            }
        }
    }
}

impl LanguageGenerator<GptNeoForCausalLM, Gpt2Vocab, Gpt2Tokenizer> for GptNeoGenerator {}
