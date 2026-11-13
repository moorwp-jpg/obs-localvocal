#ifndef WHISPER_PARAMS_STRINGS_H
#define WHISPER_PARAMS_STRINGS_H

#include <string>

#include <whisper.h>

// whisper_full_params only points to its strings. These are copies of them, for params that
// must outlive the strings they were built from: OBS frees a settings string as soon as the
// setting changes.
struct whisper_params_strings {
	std::string language;
	std::string initial_prompt;
	std::string suppress_regex;
};

// Copies the strings `params` points to into `strings`, and points `params` at the copies.
// Null strings stay null.
inline void own_whisper_params_strings(whisper_full_params &params, whisper_params_strings &strings)
{
	auto own = [](const char *&field, std::string &storage) {
		if (field == nullptr || field == storage.c_str()) {
			return;
		}
		storage = field;
		field = storage.c_str();
	};
	own(params.language, strings.language);
	own(params.initial_prompt, strings.initial_prompt);
	own(params.suppress_regex, strings.suppress_regex);
}

#endif // WHISPER_PARAMS_STRINGS_H
