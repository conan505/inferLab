#include "inferlab_runtime.h"

#include <algorithm>
#include <chrono>
#include <cmath>
#include <cctype>
#include <cstring>
#include <fstream>
#include <limits>
#include <memory>
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

struct Session {
    const Model* model;
    std::vector<std::uint32_t> context;
    std::uint32_t max_tokens;
    std::uint32_t generated = 0;
    std::uint32_t prompt_tokens;
    bool emitted_visible_token = false;
    bool finished = false;

    Session(const Model* source, const std::string& prompt, std::uint32_t maximum)
        : model(source),
          context(source->tokenize(prompt)),
          max_tokens(maximum),
          prompt_tokens(static_cast<std::uint32_t>(context.size() - 1)) {}
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
        return new Session(&checked, prompt, max_tokens);
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
        const auto scores = checked.model->forward(checked.context);
        const auto maximum =
            std::max_element(scores.begin(), scores.end()) - scores.begin();
        const auto selected = static_cast<std::uint32_t>(maximum);
        std::copy(scores.begin(), scores.end(), logits);
        *token_id = selected;
        ++checked.generated;
        checked.context.push_back(selected);
        if (checked.context.size() > checked.model->context_length) {
            checked.context.erase(checked.context.begin() + 1);
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

}  // extern "C"
