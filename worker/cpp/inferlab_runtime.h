#ifndef INFERLAB_RUNTIME_H
#define INFERLAB_RUNTIME_H

#include <cstddef>
#include <cstdint>

struct InferlabPagedCacheStats {
    std::uint64_t page_tokens;
    std::uint64_t page_count;
    std::uint64_t prefix_capacity;
    std::uint64_t page_bytes;
    std::uint64_t capacity_bytes;
    std::uint64_t allocated_pages;
    std::uint64_t free_pages;
    std::uint64_t used_token_slots;
    std::uint64_t allocated_token_slots;
    std::uint64_t internal_fragmentation_bytes;
    std::uint64_t live_references;
    std::uint64_t shared_pages;
    std::uint64_t maximum_refcount;
    std::uint64_t logical_referenced_bytes;
    std::uint64_t physical_used_bytes;
    std::uint64_t bytes_saved_by_sharing;
    std::uint64_t prefix_entries;
    std::uint64_t prefix_hits;
    std::uint64_t prefix_misses;
    std::uint64_t prefix_tokens_reused;
    std::uint64_t copy_on_write_copies;
    std::uint64_t evictions;
    std::uint64_t allocation_failures;
};

struct InferlabSamplingConfig {
    float temperature;
    std::uint32_t top_k;
    float top_p;
    float repetition_penalty;
};

struct InferlabSamplingResult {
    std::uint32_t token_id;
    std::uint32_t candidate_count;
    float selected_probability;
    float entropy;
};

struct InferlabQuantizationStats {
    std::uint64_t fp32_tensor_bytes;
    std::uint64_t active_tensor_bytes;
    std::uint64_t fp32_linear_weight_bytes;
    std::uint64_t active_linear_weight_bytes;
    std::uint64_t quantized_values;
    std::uint64_t scale_count;
    std::uint64_t zero_point_count;
    std::uint32_t mode;
    std::uint32_t group_size;
};

struct InferlabSpeculativeSampleResult {
    std::uint32_t proposed_token_id;
    std::uint32_t output_token_id;
    std::uint32_t proposal_accepted;
    float draft_probability;
    float target_probability;
};

extern "C" {

void* inferlab_model_load(const char* path, char* error, std::size_t error_capacity);
void* inferlab_model_load_with_quantization(
    const char* path,
    std::uint32_t quantization_mode,
    char* error,
    std::size_t error_capacity
);
void inferlab_model_free(void* model);

std::uint32_t inferlab_model_vocab_size(const void* model);
std::uint32_t inferlab_model_context_length(const void* model);
std::uint32_t inferlab_model_dimension(const void* model);
std::uint32_t inferlab_model_heads(const void* model);
std::uint32_t inferlab_model_feed_forward_dimension(const void* model);
int inferlab_model_quantization_stats(
    const void* model,
    InferlabQuantizationStats* stats,
    char* error,
    std::size_t error_capacity
);
int inferlab_model_configure_paged_cache(
    void* model,
    std::uint32_t page_tokens,
    std::uint32_t page_count,
    std::uint32_t prefix_capacity,
    char* error,
    std::size_t error_capacity
);
int inferlab_model_paged_cache_stats(
    const void* model,
    InferlabPagedCacheStats* stats,
    char* error,
    std::size_t error_capacity
);
int inferlab_model_token(
    const void* model,
    std::uint32_t token_id,
    char* token,
    std::size_t token_capacity,
    char* error,
    std::size_t error_capacity
);

std::int64_t inferlab_tokenize(
    const void* model,
    const char* prompt,
    std::uint32_t* token_ids,
    std::size_t token_capacity,
    char* error,
    std::size_t error_capacity
);

void* inferlab_session_create(
    const void* model,
    const void* draft_model,
    const char* prompt,
    std::uint32_t max_tokens,
    std::uint32_t cache_mode,
    std::uint64_t seed,
    std::uint32_t speculative_tokens,
    char* error,
    std::size_t error_capacity
);
void inferlab_session_free(void* session);
std::uint32_t inferlab_session_prompt_tokens(const void* session);

// Returns 1 for a visible token, 2 when EOS was selected, 0 when the length
// limit was already reached, and -1 on error.
int inferlab_session_next(
    void* session,
    const InferlabSamplingConfig* sampling,
    const std::uint32_t* banned_token_ids,
    std::size_t banned_token_count,
    const std::uint32_t* allowed_token_ids,
    std::size_t allowed_token_count,
    InferlabSamplingResult* sampling_result,
    char* piece,
    std::size_t piece_capacity,
    float* logits,
    std::size_t logits_capacity,
    std::uint64_t* duration_ns,
    char* error,
    std::size_t error_capacity
);

int inferlab_sample_logits(
    const float* logits,
    std::size_t logits_count,
    const std::uint32_t* history,
    std::size_t history_count,
    const InferlabSamplingConfig* sampling,
    const std::uint32_t* banned_token_ids,
    std::size_t banned_token_count,
    const std::uint32_t* allowed_token_ids,
    std::size_t allowed_token_count,
    std::uint64_t* random_state,
    InferlabSamplingResult* sampling_result,
    char* error,
    std::size_t error_capacity
);
int inferlab_speculative_sample_logits(
    const float* target_logits,
    const float* draft_logits,
    std::size_t logits_count,
    const std::uint32_t* history,
    std::size_t history_count,
    const InferlabSamplingConfig* sampling,
    std::uint64_t* target_random_state,
    std::uint64_t* draft_random_state,
    InferlabSpeculativeSampleResult* result,
    char* error,
    std::size_t error_capacity
);

std::uint64_t inferlab_session_query_tokens(const void* session);
std::uint64_t inferlab_session_kv_tokens(const void* session);
std::uint64_t inferlab_session_attention_score_elements(const void* session);
std::uint64_t inferlab_session_cache_bytes(const void* session);
std::uint64_t inferlab_session_peak_cache_bytes(const void* session);
std::uint64_t inferlab_session_cache_rebuilds(const void* session);
std::uint64_t inferlab_session_cache_pages(const void* session);
std::uint64_t inferlab_session_shared_cache_pages(const void* session);
std::uint64_t inferlab_session_reserved_cache_bytes(const void* session);
std::uint64_t inferlab_session_internal_fragmentation_bytes(const void* session);
std::uint32_t inferlab_session_prefix_cache_hit(const void* session);
std::uint64_t inferlab_session_prefix_tokens_reused(const void* session);
std::uint64_t inferlab_session_copy_on_write_copies(const void* session);
std::uint64_t inferlab_session_target_forward_calls(const void* session);
std::uint64_t inferlab_session_draft_forward_calls(const void* session);
std::uint64_t inferlab_session_speculative_cycles(const void* session);
std::uint64_t inferlab_session_draft_tokens_proposed(const void* session);
std::uint64_t inferlab_session_draft_tokens_accepted(const void* session);
std::uint64_t inferlab_session_draft_tokens_rejected(const void* session);
std::uint64_t inferlab_session_correction_tokens(const void* session);
std::uint64_t inferlab_session_extra_target_tokens(const void* session);

}  // extern "C"

#endif
