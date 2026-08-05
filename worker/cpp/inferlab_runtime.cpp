#include "inferlab_runtime.h"
#include "attention_cpu.h"

#include <algorithm>
#include <chrono>
#include <cmath>
#include <cctype>
#include <cstring>
#include <deque>
#include <fstream>
#include <limits>
#include <map>
#include <memory>
#include <mutex>
#include <stdexcept>
#include <string>
#include <unordered_map>
#include <utility>
#include <vector>

namespace {

constexpr char kMagic[] = {'I', 'N', 'F', 'L', 'A', 'B', '1', '\0'};
constexpr std::uint32_t kFormatVersion = 1;
constexpr std::uint32_t kBosToken = 1;
constexpr std::uint32_t kEosToken = 2;
constexpr std::uint32_t kUnknownToken = 3;
constexpr float kLayerNormEpsilon = 1.0e-5F;
constexpr std::uint32_t kDefaultPageTokens = 4;
constexpr std::uint32_t kDefaultPageCount = 64;
constexpr std::uint32_t kDefaultPrefixCapacity = 32;

std::size_t checked_product(std::size_t left, std::size_t right) {
    if (left != 0 && right > std::numeric_limits<std::size_t>::max() / left) {
        throw std::runtime_error("model dimensions overflow addressable memory");
    }
    return left * right;
}

class Reader {
public:
    explicit Reader(std::vector<unsigned char> bytes)
        : bytes_(std::move(bytes)) {}

    std::vector<unsigned char> read_bytes(std::size_t count) {
        require(count);
        std::vector<unsigned char> value(
            bytes_.begin() + static_cast<std::ptrdiff_t>(position_),
            bytes_.begin() + static_cast<std::ptrdiff_t>(position_ + count)
        );
        position_ += count;
        return value;
    }

    std::uint32_t read_u32() {
        require(4);
        const std::uint32_t value =
            static_cast<std::uint32_t>(bytes_[position_]) |
            (static_cast<std::uint32_t>(bytes_[position_ + 1]) << 8U) |
            (static_cast<std::uint32_t>(bytes_[position_ + 2]) << 16U) |
            (static_cast<std::uint32_t>(bytes_[position_ + 3]) << 24U);
        position_ += 4;
        return value;
    }

    std::string read_string() {
        const auto length = static_cast<std::size_t>(read_u32());
        const auto bytes = read_bytes(length);
        return std::string(bytes.begin(), bytes.end());
    }

    std::vector<float> read_floats(std::size_t count) {
        std::vector<float> values;
        values.reserve(count);
        for (std::size_t index = 0; index < count; ++index) {
            const std::uint32_t bits = read_u32();
            float value = 0.0F;
            static_assert(sizeof(value) == sizeof(bits));
            std::memcpy(&value, &bits, sizeof(value));
            if (!std::isfinite(value)) {
                throw std::runtime_error("model contains a non-finite weight");
            }
            values.push_back(value);
        }
        return values;
    }

    bool finished() const {
        return position_ == bytes_.size();
    }

private:
    void require(std::size_t count) const {
        if (count > bytes_.size() - position_) {
            throw std::runtime_error("model file ended before all tensors were read");
        }
    }

    std::vector<unsigned char> bytes_;
    std::size_t position_ = 0;
};

std::vector<unsigned char> read_file(const char* path) {
    if (path == nullptr || path[0] == '\0') {
        throw std::runtime_error("model path is empty");
    }
    std::ifstream input(path, std::ios::binary | std::ios::ate);
    if (!input) {
        throw std::runtime_error(std::string("cannot open model file: ") + path);
    }
    const auto end = input.tellg();
    if (end <= 0) {
        throw std::runtime_error("model file is empty");
    }
    const auto size = static_cast<std::size_t>(end);
    std::vector<unsigned char> bytes(size);
    input.seekg(0, std::ios::beg);
    input.read(
        reinterpret_cast<char*>(bytes.data()),
        static_cast<std::streamsize>(bytes.size())
    );
    if (!input) {
        throw std::runtime_error("failed to read the complete model file");
    }
    return bytes;
}

std::string lowercase_ascii(std::string value) {
    for (char& character : value) {
        const auto byte = static_cast<unsigned char>(character);
        character = static_cast<char>(std::tolower(byte));
    }
    return value;
}

struct SamplingInputs {
    const std::uint32_t* banned_token_ids;
    std::size_t banned_token_count;
    const std::uint32_t* allowed_token_ids;
    std::size_t allowed_token_count;
};

double next_uniform(std::uint64_t& state) {
    state += 0x9E3779B97F4A7C15ULL;
    std::uint64_t value = state;
    value = (value ^ (value >> 30U)) * 0xBF58476D1CE4E5B9ULL;
    value = (value ^ (value >> 27U)) * 0x94D049BB133111EBULL;
    value ^= value >> 31U;
    return static_cast<double>(value >> 11U) * (1.0 / 9007199254740992.0);
}

struct ProcessedDistribution {
    std::vector<std::size_t> tokens;
    std::vector<double> probabilities;
    double entropy = 0.0;
};

ProcessedDistribution build_distribution(
    const std::vector<float>& logits,
    const std::uint32_t* history,
    std::size_t history_count,
    const InferlabSamplingConfig& config,
    const SamplingInputs& inputs
) {
    if (logits.empty()) {
        throw std::runtime_error("sampling requires at least one logit");
    }
    if (!std::isfinite(config.temperature) || config.temperature < 0.0F ||
        config.temperature > 100.0F) {
        throw std::runtime_error("temperature must be finite and between 0 and 100");
    }
    if (!std::isfinite(config.top_p) || config.top_p <= 0.0F ||
        config.top_p > 1.0F) {
        throw std::runtime_error("top_p must be finite and in (0, 1]");
    }
    if (!std::isfinite(config.repetition_penalty) ||
        config.repetition_penalty < 1.0F || config.repetition_penalty > 100.0F) {
        throw std::runtime_error(
            "repetition_penalty must be finite and between 1 and 100"
        );
    }
    if (config.top_k > logits.size()) {
        throw std::runtime_error("top_k cannot exceed the vocabulary size");
    }
    if (history_count > 0 && history == nullptr) {
        throw std::runtime_error("sampling history pointer is null");
    }
    if (inputs.banned_token_count > 0 && inputs.banned_token_ids == nullptr) {
        throw std::runtime_error("banned-token pointer is null");
    }
    if (inputs.allowed_token_count > 0 && inputs.allowed_token_ids == nullptr) {
        throw std::runtime_error("allowed-token pointer is null");
    }

    const float negative_infinity = -std::numeric_limits<float>::infinity();
    std::vector<float> processed = logits;
    for (const float value : processed) {
        if (!std::isfinite(value)) {
            throw std::runtime_error("sampling logits must be finite");
        }
    }

    if (config.repetition_penalty != 1.0F) {
        std::vector<bool> seen(logits.size(), false);
        for (std::size_t index = 0; index < history_count; ++index) {
            const auto token = static_cast<std::size_t>(history[index]);
            if (token >= logits.size()) {
                throw std::runtime_error("sampling history token is out of range");
            }
            if (seen[token]) {
                continue;
            }
            seen[token] = true;
            if (processed[token] < 0.0F) {
                processed[token] *= config.repetition_penalty;
            } else {
                processed[token] /= config.repetition_penalty;
            }
        }
    }

    for (std::size_t index = 0; index < inputs.banned_token_count; ++index) {
        const auto token = static_cast<std::size_t>(inputs.banned_token_ids[index]);
        if (token >= logits.size()) {
            throw std::runtime_error("banned token ID is out of range");
        }
        processed[token] = negative_infinity;
    }

    if (inputs.allowed_token_count > 0) {
        std::vector<bool> allowed(logits.size(), false);
        for (std::size_t index = 0; index < inputs.allowed_token_count; ++index) {
            const auto token = static_cast<std::size_t>(inputs.allowed_token_ids[index]);
            if (token >= logits.size()) {
                throw std::runtime_error("allowed token ID is out of range");
            }
            allowed[token] = true;
        }
        for (std::size_t token = 0; token < processed.size(); ++token) {
            if (!allowed[token]) {
                processed[token] = negative_infinity;
            }
        }
    }

    if (config.temperature > 0.0F && config.temperature != 1.0F) {
        for (float& value : processed) {
            if (std::isfinite(value)) {
                value /= config.temperature;
            }
        }
    }

    auto ranked_tokens = [&processed] {
        std::vector<std::size_t> ranked;
        for (std::size_t token = 0; token < processed.size(); ++token) {
            if (std::isfinite(processed[token])) {
                ranked.push_back(token);
            }
        }
        std::sort(
            ranked.begin(),
            ranked.end(),
            [&processed](std::size_t left, std::size_t right) {
                if (processed[left] == processed[right]) {
                    return left < right;
                }
                return processed[left] > processed[right];
            }
        );
        return ranked;
    };

    auto ranked = ranked_tokens();
    if (ranked.empty()) {
        throw std::runtime_error("all tokens were masked by decoding constraints");
    }
    if (config.top_k > 0 && config.top_k < ranked.size()) {
        for (std::size_t index = config.top_k; index < ranked.size(); ++index) {
            processed[ranked[index]] = negative_infinity;
        }
        ranked.resize(config.top_k);
    }

    auto probabilities_for = [&processed](const std::vector<std::size_t>& tokens) {
        const float maximum = processed[tokens.front()];
        std::vector<double> probabilities(tokens.size());
        double denominator = 0.0;
        for (std::size_t index = 0; index < tokens.size(); ++index) {
            probabilities[index] = std::exp(
                static_cast<double>(processed[tokens[index]] - maximum)
            );
            denominator += probabilities[index];
        }
        for (double& probability : probabilities) {
            probability /= denominator;
        }
        return probabilities;
    };

    if (config.top_p < 1.0F) {
        const auto probabilities = probabilities_for(ranked);
        double cumulative = 0.0;
        std::size_t retained = 0;
        do {
            cumulative += probabilities[retained];
            ++retained;
        } while (retained < ranked.size() && cumulative < config.top_p);
        for (std::size_t index = retained; index < ranked.size(); ++index) {
            processed[ranked[index]] = negative_infinity;
        }
        ranked.resize(retained);
    }

    auto probabilities = probabilities_for(ranked);
    double entropy = 0.0;
    for (const double probability : probabilities) {
        if (probability > 0.0) {
            entropy -= probability * std::log(probability);
        }
    }
    return ProcessedDistribution{
        std::move(ranked),
        std::move(probabilities),
        entropy,
    };
}

InferlabSamplingResult select_from_distribution(
    const ProcessedDistribution& distribution,
    bool greedy,
    std::uint64_t& random_state
) {
    InferlabSamplingResult result{};
    result.candidate_count =
        static_cast<std::uint32_t>(distribution.tokens.size());
    if (greedy) {
        result.token_id = static_cast<std::uint32_t>(distribution.tokens.front());
        result.selected_probability = 1.0F;
        result.entropy = 0.0F;
        return result;
    }
    const double draw = next_uniform(random_state);
    double cumulative = 0.0;
    std::size_t selected_index = distribution.tokens.size() - 1;
    for (std::size_t index = 0; index < distribution.tokens.size(); ++index) {
        cumulative += distribution.probabilities[index];
        if (draw < cumulative) {
            selected_index = index;
            break;
        }
    }
    result.token_id =
        static_cast<std::uint32_t>(distribution.tokens[selected_index]);
    result.selected_probability =
        static_cast<float>(distribution.probabilities[selected_index]);
    result.entropy = static_cast<float>(distribution.entropy);
    return result;
}

InferlabSamplingResult select_token(
    const std::vector<float>& logits,
    const std::uint32_t* history,
    std::size_t history_count,
    const InferlabSamplingConfig& config,
    const SamplingInputs& inputs,
    std::uint64_t& random_state
) {
    const auto distribution = build_distribution(
        logits,
        history,
        history_count,
        config,
        inputs
    );
    return select_from_distribution(
        distribution,
        config.temperature == 0.0F,
        random_state
    );
}

double probability_of(
    const ProcessedDistribution& distribution,
    std::uint32_t token
) {
    for (std::size_t index = 0; index < distribution.tokens.size(); ++index) {
        if (distribution.tokens[index] == token) {
            return distribution.probabilities[index];
        }
    }
    return 0.0;
}

InferlabSamplingResult sample_residual(
    const ProcessedDistribution& target,
    const ProcessedDistribution& draft,
    std::uint64_t& random_state
) {
    ProcessedDistribution residual;
    double total = 0.0;
    for (std::size_t index = 0; index < target.tokens.size(); ++index) {
        const auto token = static_cast<std::uint32_t>(target.tokens[index]);
        const double probability = std::max(
            0.0,
            target.probabilities[index] - probability_of(draft, token)
        );
        if (probability > 0.0) {
            residual.tokens.push_back(token);
            residual.probabilities.push_back(probability);
            total += probability;
        }
    }
    if (total <= std::numeric_limits<double>::epsilon()) {
        return select_from_distribution(target, false, random_state);
    }
    residual.entropy = 0.0;
    for (double& probability : residual.probabilities) {
        probability /= total;
        residual.entropy -= probability * std::log(probability);
    }
    return select_from_distribution(residual, false, random_state);
}

enum class QuantizationMode : std::uint32_t {
    Fp32 = 0,
    Int8 = 1,
    Int4 = 2,
};

QuantizationMode quantization_mode(std::uint32_t value) {
    switch (value) {
        case 0:
            return QuantizationMode::Fp32;
        case 1:
            return QuantizationMode::Int8;
        case 2:
            return QuantizationMode::Int4;
        default:
            throw std::runtime_error(
                "quantization mode must be fp32, int8, or int4"
            );
    }
}

inferlab::AttentionAlgorithm attention_algorithm(std::uint32_t value) {
    switch (value) {
        case 0:
            return inferlab::AttentionAlgorithm::Materialized;
        case 1:
            return inferlab::AttentionAlgorithm::OnlineTiled;
        default:
            throw std::runtime_error(
                "attention algorithm must be 0 (materialized) or 1 (online tiled)"
            );
    }
}

inferlab::AttentionPrecision attention_precision(std::uint32_t value) {
    switch (value) {
        case 0:
            return inferlab::AttentionPrecision::Fp32;
        case 1:
            return inferlab::AttentionPrecision::Fp16;
        case 2:
            return inferlab::AttentionPrecision::Bf16;
        default:
            throw std::runtime_error(
                "attention precision must be 0 (FP32), 1 (FP16), or 2 (BF16)"
            );
    }
}

inferlab::AttentionConfig attention_config(
    std::uint32_t algorithm,
    std::uint32_t precision,
    std::uint32_t tile_tokens,
    bool causal = true
) {
    if (tile_tokens == 0 || tile_tokens > 4096) {
        throw std::runtime_error(
            "attention tile_tokens must be between 1 and 4096"
        );
    }
    return inferlab::AttentionConfig{
        attention_algorithm(algorithm),
        attention_precision(precision),
        tile_tokens,
        causal,
    };
}

class LinearWeight {
public:
    static constexpr std::size_t kInt4GroupSize = 8;

    static LinearWeight from_fp32(
        std::vector<float> values,
        std::size_t rows,
        std::size_t columns,
        QuantizationMode mode
    ) {
        if (values.size() != checked_product(rows, columns) ||
            rows == 0 || columns == 0) {
            throw std::runtime_error("linear weight shape is invalid");
        }
        LinearWeight result;
        result.rows_ = rows;
        result.columns_ = columns;
        result.mode_ = mode;
        if (mode == QuantizationMode::Fp32) {
            result.fp32_ = std::move(values);
            return result;
        }
        if (mode == QuantizationMode::Int8) {
            result.int8_values_.resize(values.size());
            result.scales_.resize(rows);
            for (std::size_t row = 0; row < rows; ++row) {
                float maximum = 0.0F;
                for (std::size_t column = 0; column < columns; ++column) {
                    maximum = std::max(
                        maximum,
                        std::abs(values[row * columns + column])
                    );
                }
                const float scale = maximum == 0.0F ? 1.0F : maximum / 127.0F;
                result.scales_[row] = scale;
                for (std::size_t column = 0; column < columns; ++column) {
                    const float scaled = values[row * columns + column] / scale;
                    const auto quantized = static_cast<int>(std::lround(scaled));
                    result.int8_values_[row * columns + column] =
                        static_cast<std::int8_t>(std::clamp(quantized, -127, 127));
                }
            }
            return result;
        }

        const std::size_t groups_per_row =
            (columns + kInt4GroupSize - 1) / kInt4GroupSize;
        const std::size_t groups = checked_product(rows, groups_per_row);
        result.int4_values_.assign((values.size() + 1) / 2, 0);
        result.scales_.resize(groups);
        result.zero_points_.resize(groups);
        for (std::size_t row = 0; row < rows; ++row) {
            for (std::size_t group = 0; group < groups_per_row; ++group) {
                const std::size_t begin = group * kInt4GroupSize;
                const std::size_t end = std::min(columns, begin + kInt4GroupSize);
                float minimum = 0.0F;
                float maximum = 0.0F;
                for (std::size_t column = begin; column < end; ++column) {
                    const float value = values[row * columns + column];
                    minimum = std::min(minimum, value);
                    maximum = std::max(maximum, value);
                }
                const float range = maximum - minimum;
                const float scale = range == 0.0F ? 1.0F : range / 15.0F;
                const auto zero = static_cast<std::uint8_t>(std::clamp(
                    static_cast<int>(std::lround(-minimum / scale)),
                    0,
                    15
                ));
                const std::size_t metadata = row * groups_per_row + group;
                result.scales_[metadata] = scale;
                result.zero_points_[metadata] = zero;
                for (std::size_t column = begin; column < end; ++column) {
                    const std::size_t index = row * columns + column;
                    const auto quantized = static_cast<std::uint8_t>(std::clamp(
                        static_cast<int>(
                            std::lround(values[index] / scale)
                        ) + static_cast<int>(zero),
                        0,
                        15
                    ));
                    const std::size_t packed = index / 2;
                    if (index % 2 == 0) {
                        result.int4_values_[packed] = quantized;
                    } else {
                        result.int4_values_[packed] |=
                            static_cast<std::uint8_t>(quantized << 4U);
                    }
                }
            }
        }
        return result;
    }

    float at(std::size_t row, std::size_t column) const {
        if (row >= rows_ || column >= columns_) {
            throw std::runtime_error("linear weight index is outside its shape");
        }
        const std::size_t index = row * columns_ + column;
        if (mode_ == QuantizationMode::Fp32) {
            return fp32_[index];
        }
        if (mode_ == QuantizationMode::Int8) {
            return static_cast<float>(int8_values_[index]) * scales_[row];
        }
        const std::uint8_t packed = int4_values_[index / 2];
        const std::uint8_t value = index % 2 == 0
            ? static_cast<std::uint8_t>(packed & 0x0FU)
            : static_cast<std::uint8_t>(packed >> 4U);
        const std::size_t groups_per_row =
            (columns_ + kInt4GroupSize - 1) / kInt4GroupSize;
        const std::size_t metadata =
            row * groups_per_row + column / kInt4GroupSize;
        return (
            static_cast<float>(value) -
            static_cast<float>(zero_points_[metadata])
        ) * scales_[metadata];
    }

    std::uint64_t fp32_bytes() const {
        return static_cast<std::uint64_t>(rows_) * columns_ * sizeof(float);
    }

    std::uint64_t storage_bytes() const {
        return static_cast<std::uint64_t>(
            fp32_.size() * sizeof(float) +
            int8_values_.size() * sizeof(std::int8_t) +
            int4_values_.size() * sizeof(std::uint8_t) +
            scales_.size() * sizeof(float) +
            zero_points_.size() * sizeof(std::uint8_t)
        );
    }

    std::uint64_t values() const {
        return static_cast<std::uint64_t>(rows_) * columns_;
    }

    std::uint64_t scale_count() const {
        return scales_.size();
    }

    std::uint64_t zero_point_count() const {
        return zero_points_.size();
    }

private:
    std::size_t rows_ = 0;
    std::size_t columns_ = 0;
    QuantizationMode mode_ = QuantizationMode::Fp32;
    std::vector<float> fp32_;
    std::vector<std::int8_t> int8_values_;
    std::vector<std::uint8_t> int4_values_;
    std::vector<float> scales_;
    std::vector<std::uint8_t> zero_points_;
};

class PagedKvPool {
public:
    struct PrefixLookup {
        std::vector<std::size_t> pages;
        std::size_t tokens = 0;
        bool hit = false;
    };

    PagedKvPool(
        std::size_t dimension,
        std::size_t page_tokens,
        std::size_t page_count,
        std::size_t prefix_capacity
    )
        : dimension_(dimension),
          page_tokens_(page_tokens),
          prefix_capacity_(prefix_capacity),
          pages_(page_count) {
        if (dimension_ == 0 || page_tokens_ == 0 || page_count == 0) {
            throw std::runtime_error("paged KV cache dimensions must be positive");
        }
        const std::size_t values_per_page =
            checked_product(page_tokens_, dimension_);
        free_pages_.reserve(page_count);
        for (std::size_t page_id = 0; page_id < page_count; ++page_id) {
            pages_[page_id].keys.resize(values_per_page);
            pages_[page_id].values.resize(values_per_page);
            free_pages_.push_back(page_count - page_id - 1);
        }
    }

    PrefixLookup acquire_longest_prefix(
        const std::vector<std::uint32_t>& tokens
    ) {
        std::lock_guard<std::mutex> guard(mutex_);
        auto selected = prefixes_.end();
        for (auto iterator = prefixes_.begin(); iterator != prefixes_.end(); ++iterator) {
            const auto& candidate = iterator->first;
            if (candidate.size() > tokens.size() ||
                (selected != prefixes_.end() &&
                 candidate.size() <= selected->first.size())) {
                continue;
            }
            if (std::equal(candidate.begin(), candidate.end(), tokens.begin())) {
                selected = iterator;
            }
        }
        if (selected == prefixes_.end()) {
            ++prefix_misses_;
            return {};
        }
        for (const std::size_t page_id : selected->second.pages) {
            retain_locked(page_id);
        }
        selected->second.last_used = next_clock_locked();
        ++prefix_hits_;
        prefix_tokens_reused_ += selected->first.size();
        return PrefixLookup{
            selected->second.pages,
            selected->first.size(),
            true,
        };
    }

    void append(
        std::vector<std::size_t>& block_table,
        std::size_t& cached_tokens,
        const std::vector<float>& key,
        const std::vector<float>& value,
        std::uint64_t& session_copy_on_write_copies
    ) {
        if (key.size() != dimension_ || value.size() != dimension_) {
            throw std::runtime_error("paged KV row has the wrong dimension");
        }
        std::lock_guard<std::mutex> guard(mutex_);
        const std::size_t slot = cached_tokens % page_tokens_;
        std::size_t page_id = 0;
        if (slot == 0) {
            page_id = allocate_locked();
            block_table.push_back(page_id);
        } else {
            if (block_table.empty()) {
                throw std::runtime_error("paged KV block table is empty for a tail row");
            }
            page_id = block_table.back();
            Page& page = checked_page_locked(page_id);
            if (page.used != slot) {
                throw std::runtime_error("paged KV tail occupancy is inconsistent");
            }
            while (free_pages_.empty() && !prefixes_.empty() &&
                   page.references > 1) {
                evict_oldest_locked();
            }
            if (page.references > 1) {
                const std::size_t replacement = allocate_locked();
                Page& copy = checked_page_locked(replacement);
                const std::size_t values_to_copy = slot * dimension_;
                std::copy_n(page.keys.begin(), values_to_copy, copy.keys.begin());
                std::copy_n(page.values.begin(), values_to_copy, copy.values.begin());
                copy.used = slot;
                block_table.back() = replacement;
                release_locked(page_id);
                page_id = replacement;
                ++copy_on_write_copies_;
                ++session_copy_on_write_copies;
            }
        }

        Page& page = checked_page_locked(page_id);
        if (page.references != 1 || page.used != slot) {
            throw std::runtime_error("paged KV append target is not privately writable");
        }
        const std::size_t offset = slot * dimension_;
        std::copy(key.begin(), key.end(), page.keys.begin() + offset);
        std::copy(value.begin(), value.end(), page.values.begin() + offset);
        page.used = slot + 1;
        page.last_used = next_clock_locked();
        ++cached_tokens;
    }

    void materialize(
        const std::vector<std::size_t>& block_table,
        std::size_t cached_tokens,
        std::vector<float>& keys,
        std::vector<float>& values
    ) const {
        std::lock_guard<std::mutex> guard(mutex_);
        const std::size_t expected_pages =
            (cached_tokens + page_tokens_ - 1) / page_tokens_;
        if (block_table.size() != expected_pages) {
            throw std::runtime_error("paged KV block table length is inconsistent");
        }
        keys.resize(cached_tokens * dimension_);
        values.resize(cached_tokens * dimension_);
        for (std::size_t token = 0; token < cached_tokens; ++token) {
            const std::size_t page_id = block_table[token / page_tokens_];
            const Page& page = checked_page_locked(page_id);
            const std::size_t slot = token % page_tokens_;
            if (page.used <= slot) {
                throw std::runtime_error("paged KV block table points past valid rows");
            }
            const std::size_t source = slot * dimension_;
            const std::size_t destination = token * dimension_;
            std::copy_n(
                page.keys.begin() + static_cast<std::ptrdiff_t>(source),
                dimension_,
                keys.begin() + static_cast<std::ptrdiff_t>(destination)
            );
            std::copy_n(
                page.values.begin() + static_cast<std::ptrdiff_t>(source),
                dimension_,
                values.begin() + static_cast<std::ptrdiff_t>(destination)
            );
        }
    }

    void publish_prefix(
        const std::vector<std::uint32_t>& tokens,
        const std::vector<std::size_t>& block_table,
        std::size_t cached_tokens
    ) {
        if (prefix_capacity_ == 0 || tokens.empty()) {
            return;
        }
        std::lock_guard<std::mutex> guard(mutex_);
        if (tokens.size() > cached_tokens) {
            throw std::runtime_error("cannot publish an incomplete paged KV prefix");
        }
        const std::size_t required_pages =
            (tokens.size() + page_tokens_ - 1) / page_tokens_;
        if (block_table.size() < required_pages) {
            throw std::runtime_error("paged KV prefix block table is incomplete");
        }
        std::vector<std::size_t> pages(
            block_table.begin(),
            block_table.begin() + static_cast<std::ptrdiff_t>(required_pages)
        );
        const auto existing = prefixes_.find(tokens);
        if (existing != prefixes_.end()) {
            for (const std::size_t page_id : pages) {
                retain_locked(page_id);
            }
            for (const std::size_t page_id : existing->second.pages) {
                release_locked(page_id);
            }
            existing->second.pages = std::move(pages);
            existing->second.last_used = next_clock_locked();
            return;
        }
        while (prefixes_.size() >= prefix_capacity_) {
            evict_oldest_locked();
        }
        for (const std::size_t page_id : pages) {
            retain_locked(page_id);
        }
        prefixes_.emplace(
            tokens,
            PrefixEntry{std::move(pages), next_clock_locked()}
        );
    }

    void release_table(std::vector<std::size_t>& block_table) {
        std::lock_guard<std::mutex> guard(mutex_);
        for (const std::size_t page_id : block_table) {
            release_locked(page_id);
        }
        block_table.clear();
    }

    std::uint64_t shared_pages(
        const std::vector<std::size_t>& block_table
    ) const {
        std::lock_guard<std::mutex> guard(mutex_);
        std::uint64_t shared = 0;
        for (const std::size_t page_id : block_table) {
            if (checked_page_locked(page_id).references > 1) {
                ++shared;
            }
        }
        return shared;
    }

    std::uint64_t page_tokens() const {
        return page_tokens_;
    }

    std::uint64_t page_bytes() const {
        return page_tokens_ * dimension_ * 2 * sizeof(float);
    }

    InferlabPagedCacheStats stats() const {
        std::lock_guard<std::mutex> guard(mutex_);
        InferlabPagedCacheStats result{};
        result.page_tokens = page_tokens_;
        result.page_count = pages_.size();
        result.prefix_capacity = prefix_capacity_;
        result.page_bytes = page_bytes();
        result.capacity_bytes = result.page_bytes * pages_.size();
        const std::uint64_t row_bytes = dimension_ * 2 * sizeof(float);
        for (const Page& page : pages_) {
            if (!page.allocated) {
                continue;
            }
            ++result.allocated_pages;
            result.used_token_slots += page.used;
            result.live_references += page.references;
            result.maximum_refcount =
                std::max(result.maximum_refcount, page.references);
            if (page.references > 1) {
                ++result.shared_pages;
            }
            result.physical_used_bytes += page.used * row_bytes;
            result.logical_referenced_bytes +=
                page.used * row_bytes * page.references;
        }
        result.free_pages = pages_.size() - result.allocated_pages;
        result.allocated_token_slots = result.allocated_pages * page_tokens_;
        result.internal_fragmentation_bytes =
            (result.allocated_token_slots - result.used_token_slots) * row_bytes;
        result.bytes_saved_by_sharing =
            result.logical_referenced_bytes - result.physical_used_bytes;
        result.prefix_entries = prefixes_.size();
        result.prefix_hits = prefix_hits_;
        result.prefix_misses = prefix_misses_;
        result.prefix_tokens_reused = prefix_tokens_reused_;
        result.copy_on_write_copies = copy_on_write_copies_;
        result.evictions = evictions_;
        result.allocation_failures = allocation_failures_;
        return result;
    }

private:
    struct Page {
        std::vector<float> keys;
        std::vector<float> values;
        std::size_t used = 0;
        std::uint64_t references = 0;
        std::uint64_t last_used = 0;
        bool allocated = false;
    };

    struct PrefixEntry {
        std::vector<std::size_t> pages;
        std::uint64_t last_used;
    };

    std::uint64_t next_clock_locked() const {
        return ++clock_;
    }

    Page& checked_page_locked(std::size_t page_id) {
        if (page_id >= pages_.size() || !pages_[page_id].allocated) {
            throw std::runtime_error("paged KV block table contains an invalid page");
        }
        return pages_[page_id];
    }

    const Page& checked_page_locked(std::size_t page_id) const {
        if (page_id >= pages_.size() || !pages_[page_id].allocated) {
            throw std::runtime_error("paged KV block table contains an invalid page");
        }
        return pages_[page_id];
    }

    void retain_locked(std::size_t page_id) {
        Page& page = checked_page_locked(page_id);
        ++page.references;
        page.last_used = next_clock_locked();
    }

    void release_locked(std::size_t page_id) {
        Page& page = checked_page_locked(page_id);
        if (page.references == 0) {
            throw std::runtime_error("paged KV page reference count underflow");
        }
        --page.references;
        if (page.references == 0) {
            page.allocated = false;
            page.used = 0;
            page.last_used = 0;
            free_pages_.push_back(page_id);
        }
    }

    std::size_t allocate_locked() {
        while (free_pages_.empty() && !prefixes_.empty()) {
            evict_oldest_locked();
        }
        if (free_pages_.empty()) {
            ++allocation_failures_;
            throw std::runtime_error("paged KV cache capacity exhausted");
        }
        const std::size_t page_id = free_pages_.back();
        free_pages_.pop_back();
        Page& page = pages_[page_id];
        if (page.allocated || page.references != 0) {
            throw std::runtime_error("paged KV free list contains a live page");
        }
        page.allocated = true;
        page.used = 0;
        page.references = 1;
        page.last_used = next_clock_locked();
        return page_id;
    }

    void evict_oldest_locked() {
        if (prefixes_.empty()) {
            return;
        }
        const auto oldest = std::min_element(
            prefixes_.begin(),
            prefixes_.end(),
            [](const auto& left, const auto& right) {
                return left.second.last_used < right.second.last_used;
            }
        );
        for (const std::size_t page_id : oldest->second.pages) {
            release_locked(page_id);
        }
        prefixes_.erase(oldest);
        ++evictions_;
    }

    std::size_t dimension_;
    std::size_t page_tokens_;
    std::size_t prefix_capacity_;
    mutable std::mutex mutex_;
    mutable std::uint64_t clock_ = 0;
    std::vector<Page> pages_;
    std::vector<std::size_t> free_pages_;
    std::map<std::vector<std::uint32_t>, PrefixEntry> prefixes_;
    std::uint64_t prefix_hits_ = 0;
    std::uint64_t prefix_misses_ = 0;
    std::uint64_t prefix_tokens_reused_ = 0;
    std::uint64_t copy_on_write_copies_ = 0;
    std::uint64_t evictions_ = 0;
    std::uint64_t allocation_failures_ = 0;
};

struct Model {
    std::uint32_t vocab_size = 0;
    std::uint32_t context_length = 0;
    std::uint32_t dimension = 0;
    std::uint32_t heads = 0;
    std::uint32_t feed_forward_dimension = 0;
    std::vector<std::string> vocabulary;
    std::unordered_map<std::string, std::uint32_t> token_lookup;
    QuantizationMode quantization = QuantizationMode::Fp32;
    inferlab::AttentionConfig attention{};

    std::vector<float> token_embedding;
    std::vector<float> position_embedding;
    std::vector<float> ln1_weight;
    std::vector<float> ln1_bias;
    LinearWeight query_weight;
    LinearWeight key_weight;
    LinearWeight value_weight;
    LinearWeight attention_output_weight;
    std::vector<float> ln2_weight;
    std::vector<float> ln2_bias;
    LinearWeight feed_forward_in_weight;
    std::vector<float> feed_forward_in_bias;
    LinearWeight feed_forward_out_weight;
    std::vector<float> feed_forward_out_bias;
    std::vector<float> final_norm_weight;
    std::vector<float> final_norm_bias;
    LinearWeight lm_head_weight;
    std::vector<float> lm_head_bias;
    std::unique_ptr<PagedKvPool> paged_cache;

    static std::unique_ptr<Model> load(
        const char* path,
        QuantizationMode quantization = QuantizationMode::Fp32,
        inferlab::AttentionConfig attention = {}
    ) {
        Reader reader(read_file(path));
        const auto magic = reader.read_bytes(sizeof(kMagic));
        if (!std::equal(magic.begin(), magic.end(), std::begin(kMagic))) {
            throw std::runtime_error("model magic does not match INFLAB1");
        }
        if (reader.read_u32() != kFormatVersion) {
            throw std::runtime_error("unsupported model format version");
        }

        auto model = std::make_unique<Model>();
        model->quantization = quantization;
        model->attention = attention;
        model->vocab_size = reader.read_u32();
        model->context_length = reader.read_u32();
        model->dimension = reader.read_u32();
        model->heads = reader.read_u32();
        model->feed_forward_dimension = reader.read_u32();
        const std::uint32_t layers = reader.read_u32();
        if (model->vocab_size < 4 || model->context_length < 2 ||
            model->dimension == 0 || model->heads == 0 ||
            model->feed_forward_dimension == 0 || layers != 1) {
            throw std::runtime_error("model dimensions or layer count are invalid");
        }
        if (model->dimension % model->heads != 0) {
            throw std::runtime_error("model dimension must be divisible by head count");
        }

        model->vocabulary.reserve(model->vocab_size);
        for (std::uint32_t token_id = 0; token_id < model->vocab_size; ++token_id) {
            std::string token = reader.read_string();
            if (token.empty()) {
                throw std::runtime_error("model vocabulary contains an empty token");
            }
            const std::string key = lowercase_ascii(token);
            if (!model->token_lookup.emplace(key, token_id).second) {
                throw std::runtime_error("model vocabulary contains duplicate tokens");
            }
            model->vocabulary.push_back(std::move(token));
        }

        const auto vocabulary = static_cast<std::size_t>(model->vocab_size);
        const auto context = static_cast<std::size_t>(model->context_length);
        const auto dimension = static_cast<std::size_t>(model->dimension);
        const auto feed_forward =
            static_cast<std::size_t>(model->feed_forward_dimension);
        model->token_embedding =
            reader.read_floats(checked_product(vocabulary, dimension));
        model->position_embedding =
            reader.read_floats(checked_product(context, dimension));
        model->ln1_weight = reader.read_floats(dimension);
        model->ln1_bias = reader.read_floats(dimension);
        model->query_weight = LinearWeight::from_fp32(
            reader.read_floats(checked_product(dimension, dimension)),
            dimension,
            dimension,
            quantization
        );
        model->key_weight = LinearWeight::from_fp32(
            reader.read_floats(checked_product(dimension, dimension)),
            dimension,
            dimension,
            quantization
        );
        model->value_weight = LinearWeight::from_fp32(
            reader.read_floats(checked_product(dimension, dimension)),
            dimension,
            dimension,
            quantization
        );
        model->attention_output_weight = LinearWeight::from_fp32(
            reader.read_floats(checked_product(dimension, dimension)),
            dimension,
            dimension,
            quantization
        );
        model->ln2_weight = reader.read_floats(dimension);
        model->ln2_bias = reader.read_floats(dimension);
        model->feed_forward_in_weight = LinearWeight::from_fp32(
            reader.read_floats(checked_product(feed_forward, dimension)),
            feed_forward,
            dimension,
            quantization
        );
        model->feed_forward_in_bias = reader.read_floats(feed_forward);
        model->feed_forward_out_weight = LinearWeight::from_fp32(
            reader.read_floats(checked_product(dimension, feed_forward)),
            dimension,
            feed_forward,
            quantization
        );
        model->feed_forward_out_bias = reader.read_floats(dimension);
        model->final_norm_weight = reader.read_floats(dimension);
        model->final_norm_bias = reader.read_floats(dimension);
        model->lm_head_weight = LinearWeight::from_fp32(
            reader.read_floats(checked_product(vocabulary, dimension)),
            vocabulary,
            dimension,
            quantization
        );
        model->lm_head_bias = reader.read_floats(vocabulary);
        if (!reader.finished()) {
            throw std::runtime_error("model file has unexpected trailing bytes");
        }
        model->paged_cache = std::make_unique<PagedKvPool>(
            dimension,
            kDefaultPageTokens,
            kDefaultPageCount,
            kDefaultPrefixCapacity
        );
        return model;
    }

    std::vector<std::uint32_t> tokenize(const std::string& prompt) const {
        std::vector<std::string> words;
        std::string current;
        const auto flush = [&]() {
            if (!current.empty()) {
                words.push_back(lowercase_ascii(current));
                current.clear();
            }
        };
        for (const char raw : prompt) {
            const auto character = static_cast<unsigned char>(raw);
            if (std::isalnum(character) || character == '\'') {
                current.push_back(static_cast<char>(character));
            } else {
                flush();
                if (character == '.') {
                    words.emplace_back(".");
                }
            }
        }
        flush();

        std::vector<std::uint32_t> token_ids;
        token_ids.reserve(words.size() + 1);
        token_ids.push_back(kBosToken);
        for (const auto& word : words) {
            const auto found = token_lookup.find(word);
            token_ids.push_back(
                found == token_lookup.end() ? kUnknownToken : found->second
            );
        }
        if (token_ids.size() > context_length) {
            const auto keep = static_cast<std::size_t>(context_length - 1);
            token_ids.erase(
                token_ids.begin() + 1,
                token_ids.end() - static_cast<std::ptrdiff_t>(keep)
            );
        }
        return token_ids;
    }

    std::vector<float> forward(const std::vector<std::uint32_t>& token_ids) const;
    std::vector<float> forward_all(const std::vector<std::uint32_t>& token_ids) const;
    void append_key_value(
        std::uint32_t token_id,
        std::size_t position,
        std::vector<float>& key_cache,
        std::vector<float>& value_cache
    ) const;
    std::vector<float> forward_cached(
        const std::vector<std::uint32_t>& token_ids,
        const std::vector<float>& key_cache,
        const std::vector<float>& value_cache
    ) const;
    InferlabQuantizationStats quantization_stats() const;
};

std::vector<float> layer_norm(
    const std::vector<float>& input,
    std::size_t rows,
    std::size_t columns,
    const std::vector<float>& weight,
    const std::vector<float>& bias
) {
    std::vector<float> output(input.size());
    for (std::size_t row = 0; row < rows; ++row) {
        const std::size_t offset = row * columns;
        float mean = 0.0F;
        for (std::size_t column = 0; column < columns; ++column) {
            mean += input[offset + column];
        }
        mean /= static_cast<float>(columns);
        float variance = 0.0F;
        for (std::size_t column = 0; column < columns; ++column) {
            const float centered = input[offset + column] - mean;
            variance += centered * centered;
        }
        variance /= static_cast<float>(columns);
        const float inverse_deviation =
            1.0F / std::sqrt(variance + kLayerNormEpsilon);
        for (std::size_t column = 0; column < columns; ++column) {
            output[offset + column] =
                (input[offset + column] - mean) * inverse_deviation *
                    weight[column] +
                bias[column];
        }
    }
    return output;
}

std::vector<float> linear(
    const std::vector<float>& input,
    std::size_t rows,
    std::size_t input_columns,
    const LinearWeight& weight,
    std::size_t output_columns,
    const std::vector<float>* bias = nullptr
) {
    std::vector<float> output(rows * output_columns, 0.0F);
    for (std::size_t row = 0; row < rows; ++row) {
        for (std::size_t out = 0; out < output_columns; ++out) {
            float value = bias == nullptr ? 0.0F : (*bias)[out];
            for (std::size_t in = 0; in < input_columns; ++in) {
                value += input[row * input_columns + in] * weight.at(out, in);
            }
            output[row * output_columns + out] = value;
        }
    }
    return output;
}

float gelu(float value) {
    constexpr float coefficient = 0.7978845608028654F;
    return 0.5F * value *
        (1.0F + std::tanh(coefficient * (value + 0.044715F * value * value * value)));
}

std::vector<float> Model::forward_all(
    const std::vector<std::uint32_t>& token_ids
) const {
    if (token_ids.empty() || token_ids.size() > context_length) {
        throw std::runtime_error("forward context length is invalid");
    }
    const std::size_t tokens = token_ids.size();
    const auto dimensions = static_cast<std::size_t>(dimension);
    const auto head_count = static_cast<std::size_t>(heads);
    const std::size_t head_dimension = dimensions / head_count;

    std::vector<float> hidden(tokens * dimensions);
    for (std::size_t token = 0; token < tokens; ++token) {
        if (token_ids[token] >= vocab_size) {
            throw std::runtime_error("forward input contains an invalid token ID");
        }
        for (std::size_t column = 0; column < dimensions; ++column) {
            hidden[token * dimensions + column] =
                token_embedding[
                    static_cast<std::size_t>(token_ids[token]) * dimensions + column
                ] +
                position_embedding[token * dimensions + column];
        }
    }

    const auto normalized =
        layer_norm(hidden, tokens, dimensions, ln1_weight, ln1_bias);
    const auto queries =
        linear(normalized, tokens, dimensions, query_weight, dimensions);
    const auto keys =
        linear(normalized, tokens, dimensions, key_weight, dimensions);
    const auto values =
        linear(normalized, tokens, dimensions, value_weight, dimensions);
    std::vector<float> attention_context(tokens * dimensions, 0.0F);
    inferlab::attention_forward(
        queries.data(),
        keys.data(),
        values.data(),
        tokens,
        tokens,
        head_count,
        head_dimension,
        0,
        attention,
        attention_context.data(),
        nullptr
    );

    const auto attention_output = linear(
        attention_context,
        tokens,
        dimensions,
        attention_output_weight,
        dimensions
    );
    for (std::size_t index = 0; index < hidden.size(); ++index) {
        hidden[index] += attention_output[index];
    }

    const auto feed_forward_input =
        layer_norm(hidden, tokens, dimensions, ln2_weight, ln2_bias);
    auto expanded = linear(
        feed_forward_input,
        tokens,
        dimensions,
        feed_forward_in_weight,
        feed_forward_dimension,
        &feed_forward_in_bias
    );
    for (float& value : expanded) {
        value = gelu(value);
    }
    const auto contracted = linear(
        expanded,
        tokens,
        feed_forward_dimension,
        feed_forward_out_weight,
        dimensions,
        &feed_forward_out_bias
    );
    for (std::size_t index = 0; index < hidden.size(); ++index) {
        hidden[index] += contracted[index];
    }

    const auto final_hidden =
        layer_norm(hidden, tokens, dimensions, final_norm_weight, final_norm_bias);
    return linear(
        final_hidden,
        tokens,
        dimensions,
        lm_head_weight,
        vocab_size,
        &lm_head_bias
    );
}

std::vector<float> Model::forward(
    const std::vector<std::uint32_t>& token_ids
) const {
    const auto all = forward_all(token_ids);
    const auto vocabulary = static_cast<std::size_t>(vocab_size);
    return std::vector<float>(
        all.end() - static_cast<std::ptrdiff_t>(vocabulary),
        all.end()
    );
}

void Model::append_key_value(
    std::uint32_t token_id,
    std::size_t position,
    std::vector<float>& key_cache,
    std::vector<float>& value_cache
) const {
    if (token_id >= vocab_size || position >= context_length) {
        throw std::runtime_error("KV cache input is outside the model bounds");
    }
    const auto dimensions = static_cast<std::size_t>(dimension);
    std::vector<float> hidden(dimensions);
    for (std::size_t column = 0; column < dimensions; ++column) {
        hidden[column] =
            token_embedding[static_cast<std::size_t>(token_id) * dimensions + column] +
            position_embedding[position * dimensions + column];
    }
    const auto normalized =
        layer_norm(hidden, 1, dimensions, ln1_weight, ln1_bias);
    const auto key = linear(normalized, 1, dimensions, key_weight, dimensions);
    const auto value = linear(normalized, 1, dimensions, value_weight, dimensions);
    key_cache.insert(key_cache.end(), key.begin(), key.end());
    value_cache.insert(value_cache.end(), value.begin(), value.end());
}

std::vector<float> Model::forward_cached(
    const std::vector<std::uint32_t>& token_ids,
    const std::vector<float>& key_cache,
    const std::vector<float>& value_cache
) const {
    if (token_ids.empty() || token_ids.size() > context_length) {
        throw std::runtime_error("cached forward context length is invalid");
    }
    const std::size_t tokens = token_ids.size();
    const auto dimensions = static_cast<std::size_t>(dimension);
    if (key_cache.size() != tokens * dimensions ||
        value_cache.size() != tokens * dimensions) {
        throw std::runtime_error("KV cache length does not match token context");
    }
    const auto selected_token = token_ids.back();
    if (selected_token >= vocab_size) {
        throw std::runtime_error("cached forward contains an invalid token ID");
    }

    std::vector<float> hidden(dimensions);
    const std::size_t position = tokens - 1;
    for (std::size_t column = 0; column < dimensions; ++column) {
        hidden[column] = token_embedding[
            static_cast<std::size_t>(selected_token) * dimensions + column
        ] + position_embedding[position * dimensions + column];
    }
    const auto normalized =
        layer_norm(hidden, 1, dimensions, ln1_weight, ln1_bias);
    const auto query =
        linear(normalized, 1, dimensions, query_weight, dimensions);

    const auto head_count = static_cast<std::size_t>(heads);
    const std::size_t head_dimension = dimensions / head_count;
    std::vector<float> attention_context(dimensions, 0.0F);
    inferlab::attention_forward(
        query.data(),
        key_cache.data(),
        value_cache.data(),
        1,
        tokens,
        head_count,
        head_dimension,
        position,
        attention,
        attention_context.data(),
        nullptr
    );

    const auto attention_output = linear(
        attention_context,
        1,
        dimensions,
        attention_output_weight,
        dimensions
    );
    for (std::size_t index = 0; index < hidden.size(); ++index) {
        hidden[index] += attention_output[index];
    }
    const auto feed_forward_input =
        layer_norm(hidden, 1, dimensions, ln2_weight, ln2_bias);
    auto expanded = linear(
        feed_forward_input,
        1,
        dimensions,
        feed_forward_in_weight,
        feed_forward_dimension,
        &feed_forward_in_bias
    );
    for (float& value : expanded) {
        value = gelu(value);
    }
    const auto contracted = linear(
        expanded,
        1,
        feed_forward_dimension,
        feed_forward_out_weight,
        dimensions,
        &feed_forward_out_bias
    );
    for (std::size_t index = 0; index < hidden.size(); ++index) {
        hidden[index] += contracted[index];
    }
    const auto final_hidden =
        layer_norm(hidden, 1, dimensions, final_norm_weight, final_norm_bias);
    return linear(
        final_hidden,
        1,
        dimensions,
        lm_head_weight,
        vocab_size,
        &lm_head_bias
    );
}

InferlabQuantizationStats Model::quantization_stats() const {
    const LinearWeight* matrices[] = {
        &query_weight,
        &key_weight,
        &value_weight,
        &attention_output_weight,
        &feed_forward_in_weight,
        &feed_forward_out_weight,
        &lm_head_weight,
    };
    std::uint64_t fp32_linear = 0;
    std::uint64_t active_linear = 0;
    std::uint64_t values = 0;
    std::uint64_t scales = 0;
    std::uint64_t zero_points = 0;
    for (const LinearWeight* matrix : matrices) {
        fp32_linear += matrix->fp32_bytes();
        active_linear += matrix->storage_bytes();
        values += matrix->values();
        scales += matrix->scale_count();
        zero_points += matrix->zero_point_count();
    }
    const std::vector<float>* fp32_tensors[] = {
        &token_embedding,
        &position_embedding,
        &ln1_weight,
        &ln1_bias,
        &ln2_weight,
        &ln2_bias,
        &feed_forward_in_bias,
        &feed_forward_out_bias,
        &final_norm_weight,
        &final_norm_bias,
        &lm_head_bias,
    };
    std::uint64_t unquantized = 0;
    for (const auto* tensor : fp32_tensors) {
        unquantized += tensor->size() * sizeof(float);
    }
    return InferlabQuantizationStats{
        unquantized + fp32_linear,
        unquantized + active_linear,
        fp32_linear,
        active_linear,
        quantization == QuantizationMode::Fp32 ? 0 : values,
        scales,
        zero_points,
        static_cast<std::uint32_t>(quantization),
        quantization == QuantizationMode::Int4
            ? static_cast<std::uint32_t>(LinearWeight::kInt4GroupSize)
            : 0,
    };
}

enum class CacheMode : std::uint32_t {
    Recompute = 0,
    Contiguous = 1,
    Paged = 2,
};

struct SpeculativeStep {
    std::uint32_t token_id;
    InferlabSamplingResult selection;
    std::vector<float> target_logits;
};

struct Session {
    const Model* model;
    const Model* draft_model;
    std::vector<std::uint32_t> context;
    std::vector<std::uint32_t> prompt_context;
    std::vector<float> key_cache;
    std::vector<float> value_cache;
    std::vector<std::size_t> block_table;
    std::size_t paged_cached_tokens = 0;
    std::uint32_t max_tokens;
    std::uint32_t generated = 0;
    std::uint32_t prompt_tokens;
    CacheMode cache_mode;
    bool prompt_published = false;
    bool prefix_cache_hit = false;
    bool emitted_visible_token = false;
    bool finished = false;
    std::uint64_t query_tokens = 0;
    std::uint64_t kv_tokens = 0;
    std::uint64_t attention_score_elements = 0;
    std::uint64_t peak_cache_bytes = 0;
    std::uint64_t cache_rebuilds = 0;
    std::uint64_t prefix_tokens_reused = 0;
    std::uint64_t copy_on_write_copies = 0;
    std::uint64_t random_state;
    std::uint64_t draft_random_state;
    std::uint32_t speculative_tokens;
    std::deque<SpeculativeStep> speculative_buffer;
    std::uint64_t target_forward_calls = 0;
    std::uint64_t draft_forward_calls = 0;
    std::uint64_t speculative_cycles = 0;
    std::uint64_t draft_tokens_proposed = 0;
    std::uint64_t draft_tokens_accepted = 0;
    std::uint64_t draft_tokens_rejected = 0;
    std::uint64_t correction_tokens = 0;
    std::uint64_t extra_target_tokens = 0;

    Session(
        const Model* source,
        const Model* draft,
        const std::string& prompt,
        std::uint32_t maximum,
        CacheMode mode,
        std::uint64_t seed,
        std::uint32_t draft_limit
    )
        : model(source),
          draft_model(draft),
          context(source->tokenize(prompt)),
          prompt_context(context),
          max_tokens(maximum),
          prompt_tokens(static_cast<std::uint32_t>(context.size() - 1)),
          cache_mode(mode),
          random_state(seed),
          draft_random_state(seed ^ 0xD1B54A32D192ED03ULL),
          speculative_tokens(draft_limit) {
        if ((draft_model == nullptr) != (speculative_tokens == 0)) {
            throw std::runtime_error(
                "draft model and speculative token count must be configured together"
            );
        }
        if (speculative_tokens > 8) {
            throw std::runtime_error("speculative_tokens must not exceed 8");
        }
        if (draft_model != nullptr && (
            draft_model->vocab_size != model->vocab_size ||
            draft_model->context_length != model->context_length ||
            draft_model->vocabulary != model->vocabulary
        )) {
            throw std::runtime_error(
                "draft and target models must have identical vocabulary and context"
            );
        }
        if (cache_mode == CacheMode::Paged) {
            const auto lookup = model->paged_cache->acquire_longest_prefix(context);
            block_table = lookup.pages;
            paged_cached_tokens = lookup.tokens;
            prefix_cache_hit = lookup.hit;
            prefix_tokens_reused = lookup.tokens;
            prompt_published = lookup.hit && lookup.tokens == prompt_context.size();
        }
    }

    ~Session() noexcept {
        if (cache_mode == CacheMode::Paged) {
            // An FFI destructor has no error channel. Runtime operations validate
            // ownership before this point, so cleanup must never unwind into Rust.
            try {
                model->paged_cache->release_table(block_table);
            } catch (...) {
            }
        }
    }

    std::uint64_t cache_bytes() const {
        if (cache_mode == CacheMode::Paged) {
            return paged_cached_tokens * static_cast<std::uint64_t>(model->dimension) *
                2 * sizeof(float);
        }
        return static_cast<std::uint64_t>(
            (key_cache.size() + value_cache.size()) * sizeof(float)
        );
    }

    std::uint64_t reserved_cache_bytes() const {
        if (cache_mode == CacheMode::Paged) {
            return block_table.size() * model->paged_cache->page_bytes();
        }
        return cache_bytes();
    }

    std::uint64_t internal_fragmentation_bytes() const {
        return reserved_cache_bytes() - cache_bytes();
    }

    std::uint64_t cache_pages() const {
        return cache_mode == CacheMode::Paged ? block_table.size() : 0;
    }

    std::uint64_t shared_cache_pages() const {
        return cache_mode == CacheMode::Paged
            ? model->paged_cache->shared_pages(block_table)
            : 0;
    }

    void ensure_cache() {
        const auto dimensions = static_cast<std::size_t>(model->dimension);
        if (cache_mode == CacheMode::Paged) {
            if (paged_cached_tokens > context.size()) {
                throw std::runtime_error("paged KV cache exceeds its token context");
            }
            for (std::size_t position = paged_cached_tokens;
                 position < context.size();
                 ++position) {
                std::vector<float> key;
                std::vector<float> value;
                model->append_key_value(context[position], position, key, value);
                model->paged_cache->append(
                    block_table,
                    paged_cached_tokens,
                    key,
                    value,
                    copy_on_write_copies
                );
                ++kv_tokens;
            }
            if (!prompt_published &&
                paged_cached_tokens >= prompt_context.size()) {
                model->paged_cache->publish_prefix(
                    prompt_context,
                    block_table,
                    paged_cached_tokens
                );
                prompt_published = true;
            }
            peak_cache_bytes = std::max(peak_cache_bytes, cache_bytes());
            return;
        }

        const std::size_t cached_tokens = key_cache.size() / dimensions;
        if (key_cache.size() != value_cache.size() ||
            key_cache.size() % dimensions != 0 ||
            cached_tokens > context.size()) {
            throw std::runtime_error("KV cache state is inconsistent");
        }
        for (std::size_t position = cached_tokens; position < context.size(); ++position) {
            model->append_key_value(
                context[position], position, key_cache, value_cache
            );
            ++kv_tokens;
        }
        peak_cache_bytes = std::max(peak_cache_bytes, cache_bytes());
    }

    void append_context(std::uint32_t token) {
        context.push_back(token);
        if (context.size() <= model->context_length) {
            return;
        }
        context.erase(context.begin() + 1);
        if (cache_mode == CacheMode::Contiguous) {
            key_cache.clear();
            value_cache.clear();
            ++cache_rebuilds;
        } else if (cache_mode == CacheMode::Paged) {
            model->paged_cache->release_table(block_table);
            paged_cached_tokens = 0;
            prompt_published = true;
            ++cache_rebuilds;
        }
    }

    bool speculation_enabled() const {
        return draft_model != nullptr && speculative_tokens > 0;
    }

    void plan_speculative(
        const InferlabSamplingConfig& sampling,
        const SamplingInputs& inputs
    ) {
        if (!speculative_buffer.empty()) {
            return;
        }
        if (!speculation_enabled()) {
            throw std::runtime_error("speculative decoding is not enabled");
        }
        if (inputs.allowed_token_count > 0) {
            throw std::runtime_error(
                "speculative decoding does not support grammar constraints"
            );
        }
        const std::uint32_t remaining = max_tokens - generated;
        const std::size_t available_context =
            model->context_length > context.size()
                ? model->context_length - context.size()
                : 0;
        const std::size_t proposal_limit = std::min({
            static_cast<std::size_t>(speculative_tokens),
            remaining > 1 ? static_cast<std::size_t>(remaining - 1) : 0,
            available_context,
        });
        if (proposal_limit == 0) {
            throw std::runtime_error(
                "speculative cycle needs room for one draft and one target token"
            );
        }

        std::vector<std::uint32_t> draft_context = context;
        std::vector<std::uint32_t> proposals;
        std::vector<ProcessedDistribution> draft_distributions;
        proposals.reserve(proposal_limit);
        draft_distributions.reserve(proposal_limit);
        for (std::size_t index = 0; index < proposal_limit; ++index) {
            const auto logits = draft_model->forward(draft_context);
            ++draft_forward_calls;
            const auto distribution = build_distribution(
                logits,
                draft_context.data(),
                draft_context.size(),
                sampling,
                inputs
            );
            const auto selected = select_from_distribution(
                distribution,
                sampling.temperature == 0.0F,
                draft_random_state
            );
            proposals.push_back(selected.token_id);
            draft_distributions.push_back(distribution);
            draft_context.push_back(selected.token_id);
            if (selected.token_id == kEosToken) {
                break;
            }
        }

        std::vector<std::uint32_t> verification_context = context;
        verification_context.insert(
            verification_context.end(),
            proposals.begin(),
            proposals.end()
        );
        const auto all_target_logits = model->forward_all(verification_context);
        ++target_forward_calls;
        ++speculative_cycles;
        draft_tokens_proposed += proposals.size();
        const auto verification_tokens =
            static_cast<std::uint64_t>(verification_context.size());
        query_tokens += verification_tokens;
        kv_tokens += verification_tokens;
        attention_score_elements += static_cast<std::uint64_t>(model->heads) *
            verification_tokens * (verification_tokens + 1) / 2;

        const auto vocabulary = static_cast<std::size_t>(model->vocab_size);
        const std::size_t first_position = context.size() - 1;
        bool rejected = false;
        for (std::size_t index = 0; index < proposals.size(); ++index) {
            const std::size_t offset = (first_position + index) * vocabulary;
            std::vector<float> target_logits(
                all_target_logits.begin() + static_cast<std::ptrdiff_t>(offset),
                all_target_logits.begin() +
                    static_cast<std::ptrdiff_t>(offset + vocabulary)
            );
            const std::size_t history_count = context.size() + index;
            const auto target_distribution = build_distribution(
                target_logits,
                verification_context.data(),
                history_count,
                sampling,
                inputs
            );
            InferlabSamplingResult output{};
            if (sampling.temperature == 0.0F) {
                output = select_from_distribution(
                    target_distribution,
                    true,
                    random_state
                );
                if (output.token_id != proposals[index]) {
                    ++draft_tokens_rejected;
                    ++correction_tokens;
                    rejected = true;
                } else {
                    ++draft_tokens_accepted;
                }
            } else {
                const double target_probability =
                    probability_of(target_distribution, proposals[index]);
                const double draft_probability =
                    probability_of(draft_distributions[index], proposals[index]);
                const double acceptance = draft_probability == 0.0
                    ? 1.0
                    : std::min(1.0, target_probability / draft_probability);
                if (next_uniform(random_state) < acceptance) {
                    output.token_id = proposals[index];
                    output.candidate_count = static_cast<std::uint32_t>(
                        target_distribution.tokens.size()
                    );
                    output.selected_probability =
                        static_cast<float>(target_probability);
                    output.entropy =
                        static_cast<float>(target_distribution.entropy);
                    ++draft_tokens_accepted;
                } else {
                    output = sample_residual(
                        target_distribution,
                        draft_distributions[index],
                        random_state
                    );
                    ++draft_tokens_rejected;
                    ++correction_tokens;
                    rejected = true;
                }
            }
            speculative_buffer.push_back(SpeculativeStep{
                output.token_id,
                output,
                std::move(target_logits),
            });
            if (rejected || output.token_id == kEosToken) {
                break;
            }
        }

        if (!rejected && !speculative_buffer.empty() &&
            speculative_buffer.back().token_id != kEosToken &&
            speculative_buffer.size() < remaining) {
            const std::size_t offset =
                (first_position + proposals.size()) * vocabulary;
            std::vector<float> target_logits(
                all_target_logits.begin() + static_cast<std::ptrdiff_t>(offset),
                all_target_logits.begin() +
                    static_cast<std::ptrdiff_t>(offset + vocabulary)
            );
            const auto target_distribution = build_distribution(
                target_logits,
                verification_context.data(),
                verification_context.size(),
                sampling,
                inputs
            );
            const auto output = select_from_distribution(
                target_distribution,
                sampling.temperature == 0.0F,
                random_state
            );
            speculative_buffer.push_back(SpeculativeStep{
                output.token_id,
                output,
                std::move(target_logits),
            });
            ++extra_target_tokens;
        }
        for (const auto& step : speculative_buffer) {
            append_context(step.token_id);
            if (step.token_id == kEosToken) {
                break;
            }
        }
    }
};

void write_error(char* error, std::size_t capacity, const std::string& message) {
    if (error == nullptr || capacity == 0) {
        return;
    }
    const std::size_t count = std::min(capacity - 1, message.size());
    std::memcpy(error, message.data(), count);
    error[count] = '\0';
}

void clear_error(char* error, std::size_t capacity) {
    if (error != nullptr && capacity > 0) {
        error[0] = '\0';
    }
}

void copy_text(
    const std::string& source,
    char* destination,
    std::size_t capacity
) {
    if (destination == nullptr || capacity <= source.size()) {
        throw std::runtime_error("output text buffer is too small");
    }
    std::memcpy(destination, source.data(), source.size());
    destination[source.size()] = '\0';
}

const Model& checked_model(const void* pointer) {
    if (pointer == nullptr) {
        throw std::runtime_error("model pointer is null");
    }
    return *static_cast<const Model*>(pointer);
}

Model& checked_model_mut(void* pointer) {
    if (pointer == nullptr) {
        throw std::runtime_error("model pointer is null");
    }
    return *static_cast<Model*>(pointer);
}

Session& checked_session(void* pointer) {
    if (pointer == nullptr) {
        throw std::runtime_error("session pointer is null");
    }
    return *static_cast<Session*>(pointer);
}

template <typename Function>
int protect(
    char* error,
    std::size_t error_capacity,
    Function&& function
) noexcept {
    try {
        clear_error(error, error_capacity);
        function();
        return 0;
    } catch (const std::exception& caught) {
        write_error(error, error_capacity, caught.what());
        return -1;
    } catch (...) {
        write_error(error, error_capacity, "unknown C++ runtime failure");
        return -1;
    }
}

}  // namespace

extern "C" {

void* inferlab_model_load(
    const char* path,
    char* error,
    std::size_t error_capacity
) {
    try {
        clear_error(error, error_capacity);
        return Model::load(path).release();
    } catch (const std::exception& caught) {
        write_error(error, error_capacity, caught.what());
        return nullptr;
    } catch (...) {
        write_error(error, error_capacity, "unknown C++ runtime failure");
        return nullptr;
    }
}

void* inferlab_model_load_with_quantization(
    const char* path,
    std::uint32_t quantization,
    char* error,
    std::size_t error_capacity
) {
    try {
        clear_error(error, error_capacity);
        return Model::load(path, quantization_mode(quantization)).release();
    } catch (const std::exception& caught) {
        write_error(error, error_capacity, caught.what());
        return nullptr;
    } catch (...) {
        write_error(error, error_capacity, "unknown C++ runtime failure");
        return nullptr;
    }
}

void* inferlab_model_load_with_options(
    const char* path,
    std::uint32_t quantization,
    std::uint32_t algorithm,
    std::uint32_t precision,
    std::uint32_t tile_tokens,
    std::uint32_t causal,
    char* error,
    std::size_t error_capacity
) {
    try {
        clear_error(error, error_capacity);
        return Model::load(
            path,
            quantization_mode(quantization),
            attention_config(algorithm, precision, tile_tokens, causal != 0)
        ).release();
    } catch (const std::exception& caught) {
        write_error(error, error_capacity, caught.what());
        return nullptr;
    } catch (...) {
        write_error(error, error_capacity, "unknown C++ runtime failure");
        return nullptr;
    }
}

void inferlab_model_free(void* model) {
    delete static_cast<Model*>(model);
}

std::uint32_t inferlab_model_vocab_size(const void* model) {
    return model == nullptr ? 0 : static_cast<const Model*>(model)->vocab_size;
}

std::uint32_t inferlab_model_context_length(const void* model) {
    return model == nullptr ? 0 : static_cast<const Model*>(model)->context_length;
}

std::uint32_t inferlab_model_dimension(const void* model) {
    return model == nullptr ? 0 : static_cast<const Model*>(model)->dimension;
}

std::uint32_t inferlab_model_heads(const void* model) {
    return model == nullptr ? 0 : static_cast<const Model*>(model)->heads;
}

std::uint32_t inferlab_model_feed_forward_dimension(const void* model) {
    return model == nullptr
        ? 0
        : static_cast<const Model*>(model)->feed_forward_dimension;
}

int inferlab_model_quantization_stats(
    const void* model,
    InferlabQuantizationStats* stats,
    char* error,
    std::size_t error_capacity
) {
    return protect(error, error_capacity, [&] {
        if (stats == nullptr) {
            throw std::runtime_error("quantization stats pointer is null");
        }
        *stats = checked_model(model).quantization_stats();
    });
}

int inferlab_model_attention_config(
    const void* model,
    InferlabAttentionConfig* config,
    char* error,
    std::size_t error_capacity
) {
    return protect(error, error_capacity, [&] {
        if (config == nullptr) {
            throw std::runtime_error("attention config pointer is null");
        }
        const auto& attention = checked_model(model).attention;
        *config = InferlabAttentionConfig{
            static_cast<std::uint32_t>(attention.algorithm),
            static_cast<std::uint32_t>(attention.precision),
            static_cast<std::uint32_t>(attention.tile_tokens),
            attention.causal ? 1U : 0U,
        };
    });
}

int inferlab_model_configure_paged_cache(
    void* model,
    std::uint32_t page_tokens,
    std::uint32_t page_count,
    std::uint32_t prefix_capacity,
    char* error,
    std::size_t error_capacity
) {
    return protect(error, error_capacity, [&] {
        auto& checked = checked_model_mut(model);
        if (page_tokens == 0 || page_tokens > checked.context_length) {
            throw std::runtime_error(
                "paged KV page_tokens must be between 1 and context length"
            );
        }
        if (page_count == 0 || page_count > 65'536) {
            throw std::runtime_error(
                "paged KV page_count must be between 1 and 65536"
            );
        }
        if (prefix_capacity > 100'000) {
            throw std::runtime_error(
                "paged KV prefix_capacity must not exceed 100000"
            );
        }
        checked.paged_cache = std::make_unique<PagedKvPool>(
            checked.dimension,
            page_tokens,
            page_count,
            prefix_capacity
        );
    });
}

int inferlab_model_paged_cache_stats(
    const void* model,
    InferlabPagedCacheStats* stats,
    char* error,
    std::size_t error_capacity
) {
    return protect(error, error_capacity, [&] {
        const auto& checked = checked_model(model);
        if (stats == nullptr) {
            throw std::runtime_error("paged KV stats pointer is null");
        }
        if (checked.paged_cache == nullptr) {
            throw std::runtime_error("paged KV cache is not configured");
        }
        *stats = checked.paged_cache->stats();
    });
}

int inferlab_model_token(
    const void* model,
    std::uint32_t token_id,
    char* token,
    std::size_t token_capacity,
    char* error,
    std::size_t error_capacity
) {
    return protect(error, error_capacity, [&] {
        const auto& checked = checked_model(model);
        if (token_id >= checked.vocab_size) {
            throw std::runtime_error("token ID is outside the model vocabulary");
        }
        copy_text(checked.vocabulary[token_id], token, token_capacity);
    });
}

std::int64_t inferlab_tokenize(
    const void* model,
    const char* prompt,
    std::uint32_t* token_ids,
    std::size_t token_capacity,
    char* error,
    std::size_t error_capacity
) {
    try {
        clear_error(error, error_capacity);
        if (prompt == nullptr) {
            throw std::runtime_error("prompt pointer is null");
        }
        const auto ids = checked_model(model).tokenize(prompt);
        if (token_ids == nullptr && token_capacity == 0) {
            return static_cast<std::int64_t>(ids.size());
        }
        if (token_ids == nullptr || token_capacity < ids.size()) {
            throw std::runtime_error("token output buffer is too small");
        }
        std::copy(ids.begin(), ids.end(), token_ids);
        return static_cast<std::int64_t>(ids.size());
    } catch (const std::exception& caught) {
        write_error(error, error_capacity, caught.what());
        return -1;
    } catch (...) {
        write_error(error, error_capacity, "unknown C++ runtime failure");
        return -1;
    }
}

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
) {
    try {
        clear_error(error, error_capacity);
        const auto& checked = checked_model(model);
        const Model* checked_draft = draft_model == nullptr
            ? nullptr
            : &checked_model(draft_model);
        if (prompt == nullptr) {
            throw std::runtime_error("prompt pointer is null");
        }
        if (max_tokens == 0 || max_tokens > checked.context_length) {
            throw std::runtime_error(
                "max_tokens must be between 1 and the model context length"
            );
        }
        if (cache_mode > static_cast<std::uint32_t>(CacheMode::Paged)) {
            throw std::runtime_error("unknown decoder cache mode");
        }
        return new Session(
            &checked,
            checked_draft,
            prompt,
            max_tokens,
            static_cast<CacheMode>(cache_mode),
            seed,
            speculative_tokens
        );
    } catch (const std::exception& caught) {
        write_error(error, error_capacity, caught.what());
        return nullptr;
    } catch (...) {
        write_error(error, error_capacity, "unknown C++ runtime failure");
        return nullptr;
    }
}

void inferlab_session_free(void* session) {
    delete static_cast<Session*>(session);
}

std::uint32_t inferlab_session_prompt_tokens(const void* session) {
    return session == nullptr
        ? 0
        : static_cast<const Session*>(session)->prompt_tokens;
}

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
) {
    int result = -1;
    const int protected_result = protect(error, error_capacity, [&] {
        auto& checked = checked_session(session);
        if (sampling == nullptr || sampling_result == nullptr ||
            duration_ns == nullptr) {
            throw std::runtime_error("step output pointer is null");
        }
        if (logits == nullptr || logits_capacity < checked.model->vocab_size) {
            throw std::runtime_error("logit output buffer is too small");
        }
        if (piece == nullptr || piece_capacity == 0) {
            throw std::runtime_error("piece output buffer is null or empty");
        }
        if (checked.finished || checked.generated >= checked.max_tokens) {
            piece[0] = '\0';
            result = 0;
            return;
        }

        const auto started = std::chrono::steady_clock::now();
        std::vector<float> scores;
        InferlabSamplingResult selected_result{};
        const SamplingInputs inputs{
            banned_token_ids,
            banned_token_count,
            allowed_token_ids,
            allowed_token_count,
        };
        const bool can_plan_speculative = checked.speculation_enabled() &&
            checked.max_tokens - checked.generated >= 2 &&
            checked.context.size() < checked.model->context_length;
        bool context_already_appended = false;
        if (can_plan_speculative || !checked.speculative_buffer.empty()) {
            if (checked.speculative_buffer.empty()) {
                checked.plan_speculative(*sampling, inputs);
            }
            SpeculativeStep step = std::move(checked.speculative_buffer.front());
            checked.speculative_buffer.pop_front();
            scores = std::move(step.target_logits);
            selected_result = step.selection;
            context_already_appended = true;
        } else {
            const auto tokens = static_cast<std::uint64_t>(checked.context.size());
            const auto heads = static_cast<std::uint64_t>(checked.model->heads);
            if (checked.cache_mode != CacheMode::Recompute) {
                checked.ensure_cache();
                ++checked.query_tokens;
                checked.attention_score_elements += heads * tokens;
                if (checked.cache_mode == CacheMode::Paged) {
                    std::vector<float> paged_keys;
                    std::vector<float> paged_values;
                    checked.model->paged_cache->materialize(
                        checked.block_table,
                        checked.paged_cached_tokens,
                        paged_keys,
                        paged_values
                    );
                    scores = checked.model->forward_cached(
                        checked.context, paged_keys, paged_values
                    );
                } else {
                    scores = checked.model->forward_cached(
                        checked.context, checked.key_cache, checked.value_cache
                    );
                }
            } else {
                checked.query_tokens += tokens;
                checked.kv_tokens += tokens;
                checked.attention_score_elements +=
                    heads * tokens * (tokens + 1) / 2;
                scores = checked.model->forward(checked.context);
            }
            ++checked.target_forward_calls;
            selected_result = select_token(
                scores,
                checked.context.data(),
                checked.context.size(),
                *sampling,
                inputs,
                checked.random_state
            );
        }
        const auto selected = selected_result.token_id;
        std::copy(scores.begin(), scores.end(), logits);
        *sampling_result = selected_result;
        ++checked.generated;
        if (!context_already_appended) {
            checked.append_context(selected);
        }
        const auto ended = std::chrono::steady_clock::now();
        *duration_ns = static_cast<std::uint64_t>(
            std::chrono::duration_cast<std::chrono::nanoseconds>(ended - started)
                .count()
        );

        if (selected == kEosToken) {
            checked.finished = true;
            piece[0] = '\0';
            result = 2;
            return;
        }
        const std::string& token = checked.model->vocabulary[selected];
        const bool punctuation = token == "." || token == "," || token == "!" ||
            token == "?";
        const std::string rendered =
            checked.emitted_visible_token && !punctuation ? " " + token : token;
        copy_text(rendered, piece, piece_capacity);
        checked.emitted_visible_token = true;
        result = 1;
    });
    return protected_result == 0 ? result : -1;
}

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
) {
    return protect(error, error_capacity, [&] {
        if (logits == nullptr || logits_count == 0 || sampling == nullptr ||
            random_state == nullptr || sampling_result == nullptr) {
            throw std::runtime_error("sampling input or output pointer is null");
        }
        const std::vector<float> copied_logits(logits, logits + logits_count);
        *sampling_result = select_token(
            copied_logits,
            history,
            history_count,
            *sampling,
            SamplingInputs{
                banned_token_ids,
                banned_token_count,
                allowed_token_ids,
                allowed_token_count,
            },
            *random_state
        );
    });
}

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
) {
    return protect(error, error_capacity, [&] {
        if (target_logits == nullptr || draft_logits == nullptr ||
            logits_count == 0 || sampling == nullptr ||
            target_random_state == nullptr || draft_random_state == nullptr ||
            result == nullptr) {
            throw std::runtime_error(
                "speculative sampling input or output pointer is null"
            );
        }
        const std::vector<float> target_values(
            target_logits,
            target_logits + logits_count
        );
        const std::vector<float> draft_values(
            draft_logits,
            draft_logits + logits_count
        );
        const SamplingInputs no_masks{nullptr, 0, nullptr, 0};
        const auto target = build_distribution(
            target_values,
            history,
            history_count,
            *sampling,
            no_masks
        );
        const auto draft = build_distribution(
            draft_values,
            history,
            history_count,
            *sampling,
            no_masks
        );
        const auto proposal = select_from_distribution(
            draft,
            sampling->temperature == 0.0F,
            *draft_random_state
        );
        bool accepted = false;
        InferlabSamplingResult output{};
        const double target_probability =
            probability_of(target, proposal.token_id);
        const double draft_probability = probability_of(draft, proposal.token_id);
        if (sampling->temperature == 0.0F) {
            output = select_from_distribution(target, true, *target_random_state);
            accepted = output.token_id == proposal.token_id;
        } else {
            const double acceptance = draft_probability == 0.0
                ? 1.0
                : std::min(1.0, target_probability / draft_probability);
            accepted = next_uniform(*target_random_state) < acceptance;
            output = accepted
                ? InferlabSamplingResult{
                    proposal.token_id,
                    static_cast<std::uint32_t>(target.tokens.size()),
                    static_cast<float>(target_probability),
                    static_cast<float>(target.entropy),
                }
                : sample_residual(target, draft, *target_random_state);
        }
        *result = InferlabSpeculativeSampleResult{
            proposal.token_id,
            output.token_id,
            accepted ? 1U : 0U,
            static_cast<float>(draft_probability),
            static_cast<float>(target_probability),
        };
    });
}

int inferlab_attention_forward(
    const float* queries,
    const float* keys,
    const float* values,
    std::size_t query_tokens,
    std::size_t key_value_tokens,
    std::size_t heads,
    std::size_t head_dimension,
    std::size_t query_start_position,
    const InferlabAttentionConfig* config,
    float* output,
    std::size_t output_capacity,
    InferlabAttentionStats* stats,
    char* error,
    std::size_t error_capacity
) {
    return protect(error, error_capacity, [&] {
        if (config == nullptr || stats == nullptr) {
            throw std::runtime_error("attention config or stats pointer is null");
        }
        const std::size_t output_values = checked_product(
            checked_product(query_tokens, heads),
            head_dimension
        );
        if (output == nullptr || output_capacity < output_values) {
            throw std::runtime_error("attention output buffer is too small");
        }
        const auto native_config = attention_config(
            config->algorithm,
            config->precision,
            config->tile_tokens,
            config->causal != 0
        );
        inferlab::AttentionStats native_stats{};
        inferlab::attention_forward(
            queries,
            keys,
            values,
            query_tokens,
            key_value_tokens,
            heads,
            head_dimension,
            query_start_position,
            native_config,
            output,
            &native_stats
        );
        *stats = InferlabAttentionStats{
            native_stats.score_elements,
            native_stats.masked_score_elements,
            native_stats.score_buffer_bytes,
            native_stats.working_set_bytes,
            native_stats.modeled_external_read_bytes,
            native_stats.modeled_external_write_bytes,
            native_stats.modeled_external_total_bytes,
            native_stats.key_tiles,
        };
    });
}

std::uint64_t inferlab_session_query_tokens(const void* session) {
    return session == nullptr ? 0 : static_cast<const Session*>(session)->query_tokens;
}

std::uint64_t inferlab_session_kv_tokens(const void* session) {
    return session == nullptr ? 0 : static_cast<const Session*>(session)->kv_tokens;
}

std::uint64_t inferlab_session_attention_score_elements(const void* session) {
    return session == nullptr
        ? 0
        : static_cast<const Session*>(session)->attention_score_elements;
}

std::uint64_t inferlab_session_cache_bytes(const void* session) {
    return session == nullptr ? 0 : static_cast<const Session*>(session)->cache_bytes();
}

std::uint64_t inferlab_session_peak_cache_bytes(const void* session) {
    return session == nullptr
        ? 0
        : static_cast<const Session*>(session)->peak_cache_bytes;
}

std::uint64_t inferlab_session_cache_rebuilds(const void* session) {
    return session == nullptr
        ? 0
        : static_cast<const Session*>(session)->cache_rebuilds;
}

std::uint64_t inferlab_session_cache_pages(const void* session) {
    return session == nullptr ? 0 : static_cast<const Session*>(session)->cache_pages();
}

std::uint64_t inferlab_session_shared_cache_pages(const void* session) {
    return session == nullptr
        ? 0
        : static_cast<const Session*>(session)->shared_cache_pages();
}

std::uint64_t inferlab_session_reserved_cache_bytes(const void* session) {
    return session == nullptr
        ? 0
        : static_cast<const Session*>(session)->reserved_cache_bytes();
}

std::uint64_t inferlab_session_internal_fragmentation_bytes(const void* session) {
    return session == nullptr
        ? 0
        : static_cast<const Session*>(session)->internal_fragmentation_bytes();
}

std::uint32_t inferlab_session_prefix_cache_hit(const void* session) {
    return session != nullptr && static_cast<const Session*>(session)->prefix_cache_hit
        ? 1
        : 0;
}

std::uint64_t inferlab_session_prefix_tokens_reused(const void* session) {
    return session == nullptr
        ? 0
        : static_cast<const Session*>(session)->prefix_tokens_reused;
}

std::uint64_t inferlab_session_copy_on_write_copies(const void* session) {
    return session == nullptr
        ? 0
        : static_cast<const Session*>(session)->copy_on_write_copies;
}

std::uint64_t inferlab_session_target_forward_calls(const void* session) {
    return session == nullptr
        ? 0
        : static_cast<const Session*>(session)->target_forward_calls;
}

std::uint64_t inferlab_session_draft_forward_calls(const void* session) {
    return session == nullptr
        ? 0
        : static_cast<const Session*>(session)->draft_forward_calls;
}

std::uint64_t inferlab_session_speculative_cycles(const void* session) {
    return session == nullptr
        ? 0
        : static_cast<const Session*>(session)->speculative_cycles;
}

std::uint64_t inferlab_session_draft_tokens_proposed(const void* session) {
    return session == nullptr
        ? 0
        : static_cast<const Session*>(session)->draft_tokens_proposed;
}

std::uint64_t inferlab_session_draft_tokens_accepted(const void* session) {
    return session == nullptr
        ? 0
        : static_cast<const Session*>(session)->draft_tokens_accepted;
}

std::uint64_t inferlab_session_draft_tokens_rejected(const void* session) {
    return session == nullptr
        ? 0
        : static_cast<const Session*>(session)->draft_tokens_rejected;
}

std::uint64_t inferlab_session_correction_tokens(const void* session) {
    return session == nullptr
        ? 0
        : static_cast<const Session*>(session)->correction_tokens;
}

std::uint64_t inferlab_session_extra_target_tokens(const void* session) {
    return session == nullptr
        ? 0
        : static_cast<const Session*>(session)->extra_target_tokens;
}

}  // extern "C"
