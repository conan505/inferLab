#ifndef INFERLAB_RUNTIME_H
#define INFERLAB_RUNTIME_H

#include <cstddef>
#include <cstdint>

extern "C" {

void* inferlab_model_load(const char* path, char* error, std::size_t error_capacity);
void inferlab_model_free(void* model);

std::uint32_t inferlab_model_vocab_size(const void* model);
std::uint32_t inferlab_model_context_length(const void* model);
std::uint32_t inferlab_model_dimension(const void* model);
std::uint32_t inferlab_model_heads(const void* model);
std::uint32_t inferlab_model_feed_forward_dimension(const void* model);
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
    const char* prompt,
    std::uint32_t max_tokens,
    std::uint32_t use_kv_cache,
    char* error,
    std::size_t error_capacity
);
void inferlab_session_free(void* session);
std::uint32_t inferlab_session_prompt_tokens(const void* session);

// Returns 1 for a visible token, 2 when EOS was selected, 0 when the length
// limit was already reached, and -1 on error.
int inferlab_session_next(
    void* session,
    std::uint32_t* token_id,
    char* piece,
    std::size_t piece_capacity,
    float* logits,
    std::size_t logits_capacity,
    std::uint64_t* duration_ns,
    char* error,
    std::size_t error_capacity
);

std::uint64_t inferlab_session_query_tokens(const void* session);
std::uint64_t inferlab_session_kv_tokens(const void* session);
std::uint64_t inferlab_session_attention_score_elements(const void* session);
std::uint64_t inferlab_session_cache_bytes(const void* session);
std::uint64_t inferlab_session_peak_cache_bytes(const void* session);
std::uint64_t inferlab_session_cache_rebuilds(const void* session);

}  // extern "C"

#endif
