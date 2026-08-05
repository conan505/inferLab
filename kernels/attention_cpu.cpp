#include "attention_cpu.h"

#include <algorithm>
#include <bit>
#include <cmath>
#include <cstddef>
#include <cstdint>
#include <limits>
#include <stdexcept>
#include <vector>

namespace inferlab {
namespace {

std::uint16_t float_to_half_bits(float value) {
    const std::uint32_t bits = std::bit_cast<std::uint32_t>(value);
    const std::uint32_t sign = (bits >> 16U) & 0x8000U;
    std::uint32_t exponent = (bits >> 23U) & 0xFFU;
    std::uint32_t mantissa = bits & 0x7FFFFFU;

    if (exponent == 0xFFU) {
        const std::uint32_t half_mantissa = mantissa == 0 ? 0 : 0x0200U;
        return static_cast<std::uint16_t>(sign | 0x7C00U | half_mantissa);
    }

    const int half_exponent = static_cast<int>(exponent) - 127 + 15;
    if (half_exponent >= 31) {
        return static_cast<std::uint16_t>(sign | 0x7C00U);
    }
    if (half_exponent <= 0) {
        if (half_exponent < -10) {
            return static_cast<std::uint16_t>(sign);
        }
        mantissa |= 0x800000U;
        const int shift = 14 - half_exponent;
        std::uint32_t half_mantissa = mantissa >> shift;
        const std::uint32_t remainder_mask = (1U << shift) - 1U;
        const std::uint32_t remainder = mantissa & remainder_mask;
        const std::uint32_t halfway = 1U << (shift - 1);
        if (remainder > halfway ||
            (remainder == halfway && (half_mantissa & 1U) != 0U)) {
            ++half_mantissa;
        }
        return static_cast<std::uint16_t>(sign | half_mantissa);
    }

    std::uint32_t half_mantissa = mantissa >> 13U;
    const std::uint32_t remainder = mantissa & 0x1FFFU;
    if (remainder > 0x1000U ||
        (remainder == 0x1000U && (half_mantissa & 1U) != 0U)) {
        ++half_mantissa;
        if (half_mantissa == 0x400U) {
            half_mantissa = 0;
            exponent = static_cast<std::uint32_t>(half_exponent + 1);
            if (exponent >= 31U) {
                return static_cast<std::uint16_t>(sign | 0x7C00U);
            }
            return static_cast<std::uint16_t>(
                sign | (exponent << 10U) | half_mantissa
            );
        }
    }
    return static_cast<std::uint16_t>(
        sign | (static_cast<std::uint32_t>(half_exponent) << 10U) |
        half_mantissa
    );
}

float half_bits_to_float(std::uint16_t half) {
    const std::uint32_t sign =
        (static_cast<std::uint32_t>(half) & 0x8000U) << 16U;
    std::uint32_t exponent =
        (static_cast<std::uint32_t>(half) >> 10U) & 0x1FU;
    std::uint32_t mantissa = static_cast<std::uint32_t>(half) & 0x03FFU;
    std::uint32_t bits = 0;
    if (exponent == 0) {
        if (mantissa == 0) {
            bits = sign;
        } else {
            int shift = 0;
            while ((mantissa & 0x0400U) == 0U) {
                mantissa <<= 1U;
                ++shift;
            }
            mantissa &= 0x03FFU;
            const std::uint32_t float_exponent =
                static_cast<std::uint32_t>(127 - 15 - shift);
            bits = sign | (float_exponent << 23U) | (mantissa << 13U);
        }
    } else if (exponent == 0x1FU) {
        bits = sign | 0x7F800000U | (mantissa << 13U);
    } else {
        exponent = exponent + (127U - 15U);
        bits = sign | (exponent << 23U) | (mantissa << 13U);
    }
    return std::bit_cast<float>(bits);
}

std::size_t checked_product(std::size_t left, std::size_t right) {
    if (left != 0 && right > std::numeric_limits<std::size_t>::max() / left) {
        throw std::runtime_error("attention shape product overflows");
    }
    return left * right;
}

std::uint64_t checked_u64_product(std::uint64_t left, std::uint64_t right) {
    if (left != 0 && right > std::numeric_limits<std::uint64_t>::max() / left) {
        throw std::runtime_error("attention metric product overflows");
    }
    return left * right;
}

std::size_t legal_sources(
    std::size_t query,
    std::size_t key_value_tokens,
    std::size_t query_start_position,
    bool causal
) {
    if (!causal) {
        return key_value_tokens;
    }
    return std::min(key_value_tokens, query_start_position + query + 1);
}

std::vector<float> prepare(
    const float* input,
    std::size_t count,
    AttentionPrecision precision
) {
    std::vector<float> result(count);
    for (std::size_t index = 0; index < count; ++index) {
        if (!std::isfinite(input[index])) {
            throw std::runtime_error("attention input must be finite");
        }
        result[index] = round_attention_value(input[index], precision);
    }
    return result;
}

void populate_stats(
    std::size_t query_tokens,
    std::size_t key_value_tokens,
    std::size_t heads,
    std::size_t head_dimension,
    std::size_t query_start_position,
    const AttentionConfig& config,
    AttentionStats& stats
) {
    std::uint64_t legal_pairs = 0;
    std::uint64_t key_tiles = 0;
    for (std::size_t query = 0; query < query_tokens; ++query) {
        const std::uint64_t legal = static_cast<std::uint64_t>(legal_sources(
            query,
            key_value_tokens,
            query_start_position,
            config.causal
        ));
        legal_pairs += checked_u64_product(legal, heads);
        key_tiles += checked_u64_product(
            (legal + config.tile_tokens - 1) / config.tile_tokens,
            heads
        );
    }
    const std::uint64_t all_pairs = checked_u64_product(
        checked_u64_product(query_tokens, key_value_tokens),
        heads
    );
    stats.score_elements = legal_pairs;
    stats.masked_score_elements = all_pairs - legal_pairs;
    stats.key_tiles = key_tiles;

    const std::uint64_t scalar_bytes = config.precision == AttentionPrecision::Fp32
        ? sizeof(float)
        : sizeof(std::uint16_t);
    const std::uint64_t query_bytes = checked_u64_product(
        checked_u64_product(query_tokens, heads),
        checked_u64_product(head_dimension, scalar_bytes)
    );
    const std::uint64_t key_value_bytes = checked_u64_product(
        checked_u64_product(key_value_tokens, heads),
        checked_u64_product(head_dimension, scalar_bytes)
    );
    const std::uint64_t output_bytes = checked_u64_product(
        checked_u64_product(query_tokens, heads),
        checked_u64_product(head_dimension, sizeof(float))
    );
    if (config.algorithm == AttentionAlgorithm::Materialized) {
        const std::uint64_t score_bytes =
            checked_u64_product(all_pairs, sizeof(float));
        stats.score_buffer_bytes = score_bytes;
        stats.working_set_bytes = score_bytes;
        stats.modeled_external_read_bytes = query_bytes +
            checked_u64_product(key_value_bytes, 2) +
            checked_u64_product(score_bytes, 2);
        stats.modeled_external_write_bytes =
            checked_u64_product(score_bytes, 2) + output_bytes;
    } else {
        stats.score_buffer_bytes =
            checked_u64_product(config.tile_tokens, sizeof(float));
        stats.working_set_bytes = stats.score_buffer_bytes +
            checked_u64_product(head_dimension, sizeof(float));
        const std::uint64_t query_tiles =
            (query_tokens + config.tile_tokens - 1) / config.tile_tokens;
        stats.modeled_external_read_bytes = query_bytes +
            checked_u64_product(
                checked_u64_product(query_tiles, key_value_bytes),
                2
            );
        stats.modeled_external_write_bytes = output_bytes;
    }
    stats.modeled_external_total_bytes =
        stats.modeled_external_read_bytes + stats.modeled_external_write_bytes;
}

void materialized_attention(
    const std::vector<float>& queries,
    const std::vector<float>& keys,
    const std::vector<float>& values,
    std::size_t query_tokens,
    std::size_t key_value_tokens,
    std::size_t heads,
    std::size_t head_dimension,
    std::size_t query_start_position,
    bool causal,
    float* output
) {
    const std::size_t row_count = checked_product(query_tokens, heads);
    std::vector<float> scores(
        checked_product(row_count, key_value_tokens),
        -std::numeric_limits<float>::infinity()
    );
    const float scale = 1.0F / std::sqrt(static_cast<float>(head_dimension));
    const std::size_t token_width = checked_product(heads, head_dimension);

    for (std::size_t query = 0; query < query_tokens; ++query) {
        const std::size_t sources = legal_sources(
            query,
            key_value_tokens,
            query_start_position,
            causal
        );
        for (std::size_t head = 0; head < heads; ++head) {
            const std::size_t row = query * heads + head;
            for (std::size_t source = 0; source < sources; ++source) {
                float score = 0.0F;
                for (std::size_t column = 0; column < head_dimension; ++column) {
                    const std::size_t head_offset = head * head_dimension + column;
                    score += queries[query * token_width + head_offset] *
                        keys[source * token_width + head_offset];
                }
                scores[row * key_value_tokens + source] = score * scale;
            }
        }
    }

    for (std::size_t query = 0; query < query_tokens; ++query) {
        const std::size_t sources = legal_sources(
            query,
            key_value_tokens,
            query_start_position,
            causal
        );
        for (std::size_t head = 0; head < heads; ++head) {
            const std::size_t row = query * heads + head;
            float maximum = -std::numeric_limits<float>::infinity();
            for (std::size_t source = 0; source < sources; ++source) {
                maximum = std::max(
                    maximum,
                    scores[row * key_value_tokens + source]
                );
            }
            float denominator = 0.0F;
            for (std::size_t source = 0; source < sources; ++source) {
                float& probability = scores[row * key_value_tokens + source];
                probability = std::exp(probability - maximum);
                denominator += probability;
            }
            for (std::size_t source = 0; source < sources; ++source) {
                scores[row * key_value_tokens + source] /= denominator;
            }
        }
    }

    for (std::size_t query = 0; query < query_tokens; ++query) {
        const std::size_t sources = legal_sources(
            query,
            key_value_tokens,
            query_start_position,
            causal
        );
        for (std::size_t head = 0; head < heads; ++head) {
            const std::size_t row = query * heads + head;
            for (std::size_t column = 0; column < head_dimension; ++column) {
                const std::size_t head_offset = head * head_dimension + column;
                float accumulated = 0.0F;
                for (std::size_t source = 0; source < sources; ++source) {
                    accumulated += scores[row * key_value_tokens + source] *
                        values[source * token_width + head_offset];
                }
                output[query * token_width + head_offset] = accumulated;
            }
        }
    }
}

void online_tiled_attention(
    const std::vector<float>& queries,
    const std::vector<float>& keys,
    const std::vector<float>& values,
    std::size_t query_tokens,
    std::size_t key_value_tokens,
    std::size_t heads,
    std::size_t head_dimension,
    std::size_t query_start_position,
    const AttentionConfig& config,
    float* output
) {
    const float scale = 1.0F / std::sqrt(static_cast<float>(head_dimension));
    const std::size_t token_width = checked_product(heads, head_dimension);
    std::vector<float> tile_scores(config.tile_tokens);
    std::vector<float> numerator(head_dimension);

    for (std::size_t query_tile_begin = 0;
         query_tile_begin < query_tokens;
         query_tile_begin += config.tile_tokens) {
        const std::size_t query_tile_end =
            std::min(query_tokens, query_tile_begin + config.tile_tokens);
        for (std::size_t query = query_tile_begin;
             query < query_tile_end;
             ++query) {
            const std::size_t sources = legal_sources(
                query,
                key_value_tokens,
                query_start_position,
                config.causal
            );
            for (std::size_t head = 0; head < heads; ++head) {
                std::fill(numerator.begin(), numerator.end(), 0.0F);
                float running_maximum = -std::numeric_limits<float>::infinity();
                float running_denominator = 0.0F;
                for (std::size_t tile_begin = 0;
                     tile_begin < sources;
                     tile_begin += config.tile_tokens) {
                    const std::size_t tile_end =
                        std::min(sources, tile_begin + config.tile_tokens);
                    const std::size_t tile_count = tile_end - tile_begin;
                    float tile_maximum = -std::numeric_limits<float>::infinity();
                    for (std::size_t index = 0; index < tile_count; ++index) {
                        const std::size_t source = tile_begin + index;
                        float score = 0.0F;
                        for (std::size_t column = 0;
                             column < head_dimension;
                             ++column) {
                            const std::size_t head_offset =
                                head * head_dimension + column;
                            score += queries[query * token_width + head_offset] *
                                keys[source * token_width + head_offset];
                        }
                        tile_scores[index] = score * scale;
                        tile_maximum =
                            std::max(tile_maximum, tile_scores[index]);
                    }

                    const float new_maximum =
                        std::max(running_maximum, tile_maximum);
                    const float previous_scale = std::isinf(running_maximum)
                        ? 0.0F
                        : std::exp(running_maximum - new_maximum);
                    running_denominator *= previous_scale;
                    for (float& component : numerator) {
                        component *= previous_scale;
                    }
                    for (std::size_t index = 0; index < tile_count; ++index) {
                        const std::size_t source = tile_begin + index;
                        const float probability =
                            std::exp(tile_scores[index] - new_maximum);
                        running_denominator += probability;
                        for (std::size_t column = 0;
                             column < head_dimension;
                             ++column) {
                            const std::size_t head_offset =
                                head * head_dimension + column;
                            numerator[column] += probability *
                                values[source * token_width + head_offset];
                        }
                    }
                    running_maximum = new_maximum;
                }
                if (!std::isfinite(running_denominator) ||
                    running_denominator <= 0.0F) {
                    throw std::runtime_error("online attention normalizer is invalid");
                }
                for (std::size_t column = 0; column < head_dimension; ++column) {
                    const std::size_t head_offset = head * head_dimension + column;
                    output[query * token_width + head_offset] =
                        numerator[column] / running_denominator;
                }
            }
        }
    }
}

}  // namespace

float round_attention_value(float value, AttentionPrecision precision) {
    if (precision == AttentionPrecision::Fp32) {
        return value;
    }
    if (precision == AttentionPrecision::Fp16) {
        return half_bits_to_float(float_to_half_bits(value));
    }
    const std::uint32_t bits = std::bit_cast<std::uint32_t>(value);
    const std::uint32_t rounding = 0x7FFFU + ((bits >> 16U) & 1U);
    const std::uint16_t bfloat =
        static_cast<std::uint16_t>((bits + rounding) >> 16U);
    return std::bit_cast<float>(static_cast<std::uint32_t>(bfloat) << 16U);
}

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
) {
    if (queries == nullptr || keys == nullptr || values == nullptr ||
        output == nullptr) {
        throw std::runtime_error("attention input or output pointer is null");
    }
    if (query_tokens == 0 || key_value_tokens == 0 || heads == 0 ||
        head_dimension == 0) {
        throw std::runtime_error("attention dimensions must be positive");
    }
    if (config.tile_tokens == 0 || config.tile_tokens > 4096) {
        throw std::runtime_error("attention tile_tokens must be between 1 and 4096");
    }
    if (config.causal &&
        (query_start_position >= key_value_tokens ||
         query_tokens > key_value_tokens - query_start_position)) {
        throw std::runtime_error("causal query positions exceed the KV sequence");
    }
    const std::size_t query_values = checked_product(
        checked_product(query_tokens, heads),
        head_dimension
    );
    const std::size_t key_value_values = checked_product(
        checked_product(key_value_tokens, heads),
        head_dimension
    );
    const auto prepared_queries =
        prepare(queries, query_values, config.precision);
    const auto prepared_keys = prepare(keys, key_value_values, config.precision);
    const auto prepared_values =
        prepare(values, key_value_values, config.precision);

    std::fill(output, output + query_values, 0.0F);
    if (config.algorithm == AttentionAlgorithm::Materialized) {
        materialized_attention(
            prepared_queries,
            prepared_keys,
            prepared_values,
            query_tokens,
            key_value_tokens,
            heads,
            head_dimension,
            query_start_position,
            config.causal,
            output
        );
    } else {
        online_tiled_attention(
            prepared_queries,
            prepared_keys,
            prepared_values,
            query_tokens,
            key_value_tokens,
            heads,
            head_dimension,
            query_start_position,
            config,
            output
        );
    }
    if (stats != nullptr) {
        *stats = AttentionStats{};
        populate_stats(
            query_tokens,
            key_value_tokens,
            heads,
            head_dimension,
            query_start_position,
            config,
            *stats
        );
    }
}

}  // namespace inferlab
