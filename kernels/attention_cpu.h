#pragma once

#include <cstddef>
#include <cstdint>

namespace inferlab {

enum class AttentionAlgorithm : std::uint32_t {
    Materialized = 0,
    OnlineTiled = 1,
};

enum class AttentionPrecision : std::uint32_t {
    Fp32 = 0,
    Fp16 = 1,
    Bf16 = 2,
};

struct AttentionConfig {
    AttentionAlgorithm algorithm = AttentionAlgorithm::Materialized;
    AttentionPrecision precision = AttentionPrecision::Fp32;
    std::size_t tile_tokens = 16;
    bool causal = true;
};

struct AttentionStats {
    std::uint64_t score_elements = 0;
    std::uint64_t masked_score_elements = 0;
    std::uint64_t score_buffer_bytes = 0;
    std::uint64_t working_set_bytes = 0;
    std::uint64_t modeled_external_read_bytes = 0;
    std::uint64_t modeled_external_write_bytes = 0;
    std::uint64_t modeled_external_total_bytes = 0;
    std::uint64_t key_tiles = 0;
};

float round_attention_value(float value, AttentionPrecision precision);

void attention_forward(
    const float* queries,
    const float* keys,
    const float* values,
    std::size_t query_tokens,
    std::size_t key_value_tokens,
    std::size_t heads,
    std::size_t head_dimension,
    std::size_t query_start_position,
    const AttentionConfig& config,
    float* output,
    AttentionStats* stats
);

}  // namespace inferlab
