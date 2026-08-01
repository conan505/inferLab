#include "inferlab_runtime.h"

#include <algorithm>
#include <chrono>
#include <cmath>
#include <cctype>
#include <cstring>
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

    std::vector<float> token_embedding;
    std::vector<float> position_embedding;
    std::vector<float> ln1_weight;
    std::vector<float> ln1_bias;
    std::vector<float> query_weight;
    std::vector<float> key_weight;
    std::vector<float> value_weight;
    std::vector<float> attention_output_weight;
    std::vector<float> ln2_weight;
    std::vector<float> ln2_bias;
    std::vector<float> feed_forward_in_weight;
    std::vector<float> feed_forward_in_bias;
    std::vector<float> feed_forward_out_weight;
    std::vector<float> feed_forward_out_bias;
    std::vector<float> final_norm_weight;
    std::vector<float> final_norm_bias;
    std::vector<float> lm_head_weight;
    std::vector<float> lm_head_bias;
    std::unique_ptr<PagedKvPool> paged_cache;

    static std::unique_ptr<Model> load(const char* path) {
        Reader reader(read_file(path));
        const auto magic = reader.read_bytes(sizeof(kMagic));
        if (!std::equal(magic.begin(), magic.end(), std::begin(kMagic))) {
            throw std::runtime_error("model magic does not match INFLAB1");
        }
        if (reader.read_u32() != kFormatVersion) {
            throw std::runtime_error("unsupported model format version");
        }

        auto model = std::make_unique<Model>();
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
        model->query_weight =
            reader.read_floats(checked_product(dimension, dimension));
        model->key_weight =
            reader.read_floats(checked_product(dimension, dimension));
        model->value_weight =
            reader.read_floats(checked_product(dimension, dimension));
        model->attention_output_weight =
            reader.read_floats(checked_product(dimension, dimension));
        model->ln2_weight = reader.read_floats(dimension);
        model->ln2_bias = reader.read_floats(dimension);
        model->feed_forward_in_weight =
            reader.read_floats(checked_product(feed_forward, dimension));
        model->feed_forward_in_bias = reader.read_floats(feed_forward);
        model->feed_forward_out_weight =
            reader.read_floats(checked_product(dimension, feed_forward));
        model->feed_forward_out_bias = reader.read_floats(dimension);
        model->final_norm_weight = reader.read_floats(dimension);
        model->final_norm_bias = reader.read_floats(dimension);
        model->lm_head_weight =
            reader.read_floats(checked_product(vocabulary, dimension));
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
    const std::vector<float>& weight,
    std::size_t output_columns,
    const std::vector<float>* bias = nullptr
) {
    std::vector<float> output(rows * output_columns, 0.0F);
    for (std::size_t row = 0; row < rows; ++row) {
        for (std::size_t out = 0; out < output_columns; ++out) {
            float value = bias == nullptr ? 0.0F : (*bias)[out];
            for (std::size_t in = 0; in < input_columns; ++in) {
                value += input[row * input_columns + in] *
                    weight[out * input_columns + in];
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

std::vector<float> Model::forward(
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
    const float scale = 1.0F / std::sqrt(static_cast<float>(head_dimension));

    for (std::size_t token = 0; token < tokens; ++token) {
        for (std::size_t head = 0; head < head_count; ++head) {
            std::vector<float> scores(token + 1, 0.0F);
            float maximum = -std::numeric_limits<float>::infinity();
            for (std::size_t source = 0; source <= token; ++source) {
                float score = 0.0F;
                for (std::size_t column = 0; column < head_dimension; ++column) {
                    const std::size_t offset = head * head_dimension + column;
                    score += queries[token * dimensions + offset] *
                        keys[source * dimensions + offset];
                }
                scores[source] = score * scale;
                maximum = std::max(maximum, scores[source]);
            }
            float denominator = 0.0F;
            for (float& score : scores) {
                score = std::exp(score - maximum);
                denominator += score;
            }
            for (float& score : scores) {
                score /= denominator;
            }
            for (std::size_t column = 0; column < head_dimension; ++column) {
                const std::size_t offset = head * head_dimension + column;
                float value = 0.0F;
                for (std::size_t source = 0; source <= token; ++source) {
                    value += scores[source] * values[source * dimensions + offset];
                }
                attention_context[token * dimensions + offset] = value;
            }
        }
    }

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
    std::vector<float> last_hidden(
        final_hidden.end() - static_cast<std::ptrdiff_t>(dimensions),
        final_hidden.end()
    );
    return linear(
        last_hidden,
        1,
        dimensions,
        lm_head_weight,
        vocab_size,
        &lm_head_bias
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
    const float scale = 1.0F / std::sqrt(static_cast<float>(head_dimension));
    std::vector<float> attention_context(dimensions, 0.0F);
    for (std::size_t head = 0; head < head_count; ++head) {
        std::vector<float> scores(tokens, 0.0F);
        float maximum = -std::numeric_limits<float>::infinity();
        for (std::size_t source = 0; source < tokens; ++source) {
            float score = 0.0F;
            for (std::size_t column = 0; column < head_dimension; ++column) {
                const std::size_t offset = head * head_dimension + column;
                score += query[offset] * key_cache[source * dimensions + offset];
            }
            scores[source] = score * scale;
            maximum = std::max(maximum, scores[source]);
        }
        float denominator = 0.0F;
        for (float& score : scores) {
            score = std::exp(score - maximum);
            denominator += score;
        }
        for (float& score : scores) {
            score /= denominator;
        }
        for (std::size_t column = 0; column < head_dimension; ++column) {
            const std::size_t offset = head * head_dimension + column;
            float value = 0.0F;
            for (std::size_t source = 0; source < tokens; ++source) {
                value += scores[source] * value_cache[source * dimensions + offset];
            }
            attention_context[offset] = value;
        }
    }

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

enum class CacheMode : std::uint32_t {
    Recompute = 0,
    Contiguous = 1,
    Paged = 2,
};

struct Session {
    const Model* model;
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

    Session(
        const Model* source,
        const std::string& prompt,
        std::uint32_t maximum,
        CacheMode mode
    )
        : model(source),
          context(source->tokenize(prompt)),
          prompt_context(context),
          max_tokens(maximum),
          prompt_tokens(static_cast<std::uint32_t>(context.size() - 1)),
          cache_mode(mode) {
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
    const char* prompt,
    std::uint32_t max_tokens,
    std::uint32_t cache_mode,
    char* error,
    std::size_t error_capacity
) {
    try {
        clear_error(error, error_capacity);
        const auto& checked = checked_model(model);
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
            prompt,
            max_tokens,
            static_cast<CacheMode>(cache_mode)
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
    std::uint32_t* token_id,
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
        if (token_id == nullptr || duration_ns == nullptr) {
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
        const auto maximum =
            std::max_element(scores.begin(), scores.end()) - scores.begin();
        const auto selected = static_cast<std::uint32_t>(maximum);
        std::copy(scores.begin(), scores.end(), logits);
        *token_id = selected;
        ++checked.generated;
        checked.context.push_back(selected);
        if (checked.context.size() > checked.model->context_length) {
            checked.context.erase(checked.context.begin() + 1);
            if (checked.cache_mode == CacheMode::Contiguous) {
                checked.key_cache.clear();
                checked.value_cache.clear();
                ++checked.cache_rebuilds;
            } else if (checked.cache_mode == CacheMode::Paged) {
                checked.model->paged_cache->release_table(checked.block_table);
                checked.paged_cached_tokens = 0;
                ++checked.cache_rebuilds;
            }
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

}  // extern "C"
