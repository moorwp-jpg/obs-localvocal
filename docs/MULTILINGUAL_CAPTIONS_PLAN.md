# Multilingual Captions Implementation Plan

Status: **Approved for implementation in the fork**  
Repository: `moorwp-jpg/obs-localvocal`  
Initial scope: up to **5 simultaneous caption languages**  
Last updated: 2026-09-13

## 1. Goal

Make multilingual caption generation a first-class LocalVocal capability instead of a special case tied to one translation output.

A single Whisper transcription should be reusable across multiple target languages, and each language should be independently routable to one or more outputs:

- OBS text sources
- `.txt` / `.srt` subtitle files
- WebVTT tracks for recording/streaming where supported
- live closed-caption output
- future caption transports without changing the transcription/translation core

The plugin should run speech recognition **once per audio segment**, then fan the resulting caption event out to the configured translation pipelines and output sinks.

The design must remain useful even when a streaming platform accepts fewer caption tracks than LocalVocal can generate. Platform restrictions belong at the final output/transport layer, not in the multilingual generation core.

## 2. Current state

The current code already contains most of the individual pieces needed for this feature, but they are configured around single targets:

- `src/transcription-filter-data.h`
  - one local translation target: `target_lang`
  - one local translation output: `translation_output`
  - one cloud target: `translate_cloud_target_language`
  - one cloud output: `translate_cloud_output`
  - one main text source: `text_source_name`
  - one main file path: `output_file_path`
  - WebVTT already has `language_to_track`, `active_languages`, and `MAX_WEBVTT_TRACKS = 5`
- `src/transcription-filter-callbacks.cpp`
  - `send_caption_to_source()` updates a named OBS text source
  - `send_sentence_to_file()` writes TXT/SRT output
  - `send_translated_sentence_to_file()` already demonstrates language-suffixed file output
  - `send_caption_to_stream()` uses the OBS caption-output path
  - `send_caption_to_webvtt()` already routes by language when a WebVTT track exists
  - local/cloud translation currently use separate single-target flows
- `src/transcription-filter-properties.cpp`
  - local and cloud translation settings are exposed as one target/output pair each
  - file output is configured as a single base output

This plan should reuse those mechanisms where practical rather than replacing the transcription engine or rewriting the plugin.

## 3. Design principles

1. **Whisper runs once.** Never run one transcription model per target language.
2. **Separate generation from routing.** Translation backends generate localized captions; output sinks decide where each caption goes.
3. **One caption identity across languages.** All translations of a caption keep the same sequence ID and timing metadata.
4. **Preserve ordering per language.** A slow translation must not cause subtitle file or track order to become `50, 52, 51`.
5. **Bound concurrency and memory.** Do not create an unbounded detached thread for every language/caption pair.
6. **Fail independently.** Failure of one translation or one sink must not suppress the original caption or unrelated languages.
7. **Backward compatible by default.** Existing configurations should continue to behave as they do today.
8. **Capability-aware streaming.** Generate all configured languages internally, but only send tracks that the selected OBS/output/platform path can actually carry.
9. **Keep the first release maintainable.** Use a maximum of five active languages initially, matching the existing WebVTT track limit.

## 4. Target architecture

```text
Audio
  |
  v
Whisper (one pass)
  |
  v
CaptionEvent (original language, text, timing, sequence ID)
  |
  +-----------------------+
  |                       |
  v                       v
Original route      Translation dispatcher
                          |
              +-----------+-----------+-----------+
              |           |           |           |
              v           v           v           v
             ES          FR          DE          JA
              |           |           |           |
              +-----------+-----------+-----------+
                          |
                          v
                    Caption router
                          |
        +-----------------+-----------------+
        |                 |                 |
        v                 v                 v
   OBS text sinks     File sinks      Stream/record sinks
```

### 4.1 Core caption types

Introduce a small model that is independent of OBS output details. Exact names can change during implementation.

```cpp
struct CaptionEvent {
    uint64_t sequence_id;
    uint64_t start_timestamp_ms;
    uint64_t end_timestamp_ms;
    std::string source_language;
    std::string text;
    bool partial;
};

struct LocalizedCaption {
    uint64_t sequence_id;
    uint64_t start_timestamp_ms;
    uint64_t end_timestamp_ms;
    std::string source_language;
    std::string language;
    std::string text;
    bool partial;
    bool is_original;
};
```

`DetectionResultWithText` can remain the Whisper-facing structure. The new types should be the stable boundary between transcription/translation and output routing.

### 4.2 Language configuration

Replace the internal assumption of one local target and one cloud target with a collection of language routes.

Conceptually:

```cpp
enum class TranslationBackend {
    Original,
    Local,
    Cloud,
};

struct CaptionLanguageConfig {
    std::string language;
    TranslationBackend backend;
    bool enabled;

    // Output routing
    std::string obs_text_source;
    bool write_file;
    bool webvtt;
    bool live_caption;
};
```

Backend-specific configuration should remain separate from language/output routing. For example, cloud provider credentials/configuration should not be copied into every output sink.

### 4.3 Output sinks

Treat output types as independent consumers of `LocalizedCaption`:

- `ObsTextSink`
- `CaptionFileSink`
- `WebVttSink`
- `LiveCaptionSink`

A language may feed several sinks at once, and one sink class may have several configured language instances.

Example:

```text
Spanish
  -> OBS source "Captions - Spanish"
  -> captions.es.srt
  -> WebVTT Spanish track
  -> live caption transport (only if supported/selected)
```

## 5. Compatibility and settings migration

Existing users must not need to recreate their configuration.

On load:

1. If the new multilingual configuration exists, use it.
2. Otherwise, synthesize routes from the existing settings:
   - original transcription -> current `text_source_name`, file, stream, and WebVTT behavior
   - existing local translation -> one migrated local language route
   - existing cloud translation -> one migrated cloud language route
3. Continue writing/reading legacy settings for at least the first implementation release if needed to avoid breaking downgrade scenarios.
4. Do not silently enable additional translations for existing users.

The first implementation should support **five configured languages total, including the original language**. This keeps the UI bounded and aligns with `MAX_WEBVTT_TRACKS`.

## 6. Translation execution model

### Requirements

- one transcription event may schedule several translation jobs
- jobs are bounded by a configurable/internal worker limit
- each job carries `sequence_id` and timestamps
- completed translations are reordered per language before ordered sinks consume them
- shutting down/removing the filter cancels or drains outstanding work safely
- no translation worker may dereference destroyed `transcription_filter_data`
- errors/timeouts affect only the failed target language

### Recommended implementation

Add a managed translation dispatcher owned by the filter instance instead of expanding the current detached-thread pattern.

Suggested responsibilities:

```text
TranslationDispatcher
  - accepts CaptionEvent
  - determines configured target languages
  - submits bounded translation jobs
  - caches duplicate full-sentence requests where safe
  - preserves per-language sequence order
  - publishes LocalizedCaption results to CaptionRouter
  - stops/join workers during filter destruction/reconfiguration
```

Use the existing local `translate()` and cloud `translate_cloud()` implementations as backend adapters. Do not rewrite translation providers unless required for thread safety.

### Partial captions

Initial behavior:

- original-language partial captions continue to work as they do today
- translated partial captions are **optional/off by default**
- full-sentence translations are the required first milestone

Reason: translating rapidly changing partial text across several languages can multiply load, API cost, flicker, and out-of-order completion. Partial multilingual translation can be enabled later after the full-sentence path is stable.

## 7. Output behavior

### 7.1 OBS text sources

Allow each configured language to select a different OBS text source.

Requirements:

- update each source independently
- no language may overwrite another language's selected source unless the user intentionally chooses the same source
- detect duplicate source assignments and warn in the UI/log
- preserve current buffered-output behavior where practical; if buffering is language-specific, move buffering state into the language route rather than sharing one translation monitor
- clearing captions must clear every configured OBS sink safely

### 7.2 Caption files

Support simultaneous per-language TXT/SRT output.

Recommended default naming from a selected base path:

```text
captions.srt       -> original language (backward-compatible option)
captions.es.srt    -> Spanish
captions.fr.srt    -> French
captions.de.srt    -> German
```

Alternative explicit paths may be added later; the first release can derive filenames automatically.

Requirements:

- all language files reuse the source caption timestamps
- each language maintains its own ordered sequence state
- recording start/reset behavior resets all configured SRT streams consistently
- `truncate_output_file`, `only_while_recording`, and recording-name matching apply consistently
- one file write failure must not stop other sinks
- sanitize/validate generated language suffixes before path construction

### 7.3 WebVTT / recording tracks

Build on the existing `MAX_WEBVTT_TRACKS`, `active_languages`, and `language_to_track` implementation.

Requirements:

- map the configured original + translated languages to stable tracks
- preserve track identity for the lifetime of an output
- reject duplicate/invalid language mappings clearly
- ensure late translations still use the original caption timestamps
- define behavior when more languages are configured than the selected output can mux
- keep streaming and recording enablement independent

The initial multilingual limit should remain 5 until the WebVTT/output limits are intentionally revisited.

### 7.4 Live closed captions / RTMP

This feature must be split into **generation capability** and **transport capability**.

LocalVocal should always be able to generate all configured languages. A live output may expose:

- `SingleTrack` - current OBS caption path / one selected language
- `MultiTrackWebVTT` - where the OBS output/container path supports the existing WebVTT mechanism
- future embedded multi-service 608/708 or platform-specific transport if OBS and the destination support it

Do not hardcode an assumption that YouTube, Twitch, Facebook, or another service accepts every generated track.

For the first release:

1. preserve the existing single live-caption path
2. let the user choose which generated language feeds that path
3. send all configured languages to WebVTT-capable outputs where supported
4. expose a clear UI note when the selected live transport is single-track
5. keep the transport interface extensible so future multi-track RTMP/CEA work does not require changes to translation or routing

A later task should research OBS/libobs support for multiple embedded CEA-608/708 services and verify actual ingestion behavior on target services before claiming selectable multilingual live CC support.

## 8. UI proposal

Keep the main settings understandable by separating **Languages** from **Outputs** conceptually, even if OBS property controls require a simpler implementation.

Initial UI can use five fixed language rows to avoid building a custom dynamic editor immediately.

Example:

```text
Multilingual Captions

[ ] Enable multilingual captions

#  Language   Generation     OBS text source       File   WebVTT
1  English    Original       Captions - English     [x]    [x]
2  Spanish    Local          Captions - Spanish     [x]    [x]
3  French     Local          Captions - French      [x]    [x]
4  German     Cloud          Captions - German      [x]    [x]
5  Off

Live caption language: [English]
```

UI requirements:

- original language must be identifiable and cannot accidentally request translation to itself
- each target language must be unique
- unsupported backend/language combinations must be disabled or rejected
- duplicate OBS target sources should warn
- cloud API/provider settings remain centralized
- existing single-target controls remain functional during migration or are cleanly hidden only when multilingual mode is enabled

## 9. Implementation tasks

Each task should be small enough to review/test independently. Prefer one PR per task or closely related pair.

### MLC-001 - Baseline, design types, and regression coverage

**Purpose:** Create the safe foundation without changing user-visible behavior.

Changes:

- add `CaptionEvent`, `LocalizedCaption`, and language-route types
- assign monotonically increasing caption sequence IDs
- add focused unit/helper tests where possible
- document existing single-language behavior that must remain unchanged
- identify lifecycle boundaries for translation workers and output callbacks

Likely files:

- `src/transcription-filter-data.h`
- new caption model/router headers under `src/`
- `src/transcription-filter-callbacks.cpp`
- `src/tests/`

Acceptance:

- existing transcription, translation, file, OBS source, stream caption, and WebVTT behavior remains unchanged
- every completed caption can be represented as a `CaptionEvent`

### MLC-002 - Multilingual configuration model and legacy migration

**Purpose:** Represent up to five language routes without changing translation execution yet.

Changes:

- store a collection of language configurations
- parse/save new settings
- migrate legacy local/cloud translation settings in memory
- validate unique languages and allowed backend combinations
- add configuration serialization tests where practical

Acceptance:

- an old OBS scene/profile loads with the same effective behavior
- a new configuration can describe original + multiple target languages
- invalid duplicate routes fail safely with useful logging

### MLC-003 - Managed multi-target translation dispatcher

**Purpose:** Generate several translations from one transcription safely.

Changes:

- add bounded worker/queue management
- adapt local and cloud translators behind the dispatcher
- remove multilingual dependence on raw detached translation threads
- attach sequence/timing metadata to every translation result
- add per-language ordering/reorder handling
- isolate failures and cancellation

Acceptance:

- one full caption can produce several target-language results
- Whisper runs only once
- slow completion in one language cannot reorder that language's final output
- filter shutdown/reconfiguration does not leave workers using destroyed state
- queue size remains bounded under sustained load

### MLC-004 - Caption router and sink interface

**Purpose:** Decouple caption generation from destination-specific output code.

Changes:

- add a router that accepts `LocalizedCaption`
- define sink interface/dispatch functions
- adapt current original/local/cloud output paths through the router
- add per-sink error isolation

Acceptance:

- one language can feed multiple sinks
- multiple languages can feed instances of the same sink type
- sink failure does not stop unrelated outputs

### MLC-005 - Multiple OBS text-source outputs

**Purpose:** Show several languages simultaneously in OBS.

Changes:

- route each configured language to its selected text source
- add per-language buffered state if needed
- update clear/reset behavior
- validate duplicate source assignments

Acceptance:

- at least original + four translated text sources can update simultaneously
- clearing captions clears all active mapped sources
- existing one-source behavior remains compatible

### MLC-006 - Multiple TXT/SRT caption files

**Purpose:** Write synchronized language-specific files.

Changes:

- create file sink instances per language
- derive safe language-specific filenames
- maintain ordered per-language SRT numbering
- preserve recording/truncation/rename semantics

Acceptance:

- original + translated SRT/TXT outputs are written simultaneously
- timestamps align across languages for the same sequence ID
- no file contains out-of-order sequence entries
- one file error does not interrupt other languages

### MLC-007 - Multi-language WebVTT / recording track integration

**Purpose:** Connect the new pipeline to the existing language-to-track muxer.

Changes:

- populate WebVTT track mapping from multilingual configuration
- route translated `LocalizedCaption` objects directly to their configured language tracks
- validate track count and duplicates
- verify recording and streaming modes independently

Acceptance:

- up to five configured language tracks can be muxed where the output supports them
- track/language mapping is stable during an output session
- original and translations share aligned timestamps

### MLC-008 - Live caption transport abstraction and single-track selection

**Purpose:** Make livestream behavior explicit and future-proof.

Changes:

- wrap the current `obs_output_output_caption_text2()` path as a `SingleTrack` live sink
- allow selection of any generated language for that sink
- preserve existing behavior when multilingual mode is off
- add transport capability enum/state for future multi-track transports
- add logs/UI messaging for unsupported multi-track live output

Acceptance:

- any generated language can be selected as the traditional live caption feed
- selecting a language does not disable generation/output of the others
- no claim of multi-track support is made unless the active transport actually supports it

### MLC-009 - Properties/UI and localization

**Purpose:** Make the feature usable without making settings difficult to maintain.

Changes:

- add multilingual enable/configuration controls
- use up to five language rows for the first release
- connect backend/source/file/WebVTT selections to the configuration model
- add validation messages/help text
- update locale strings

Acceptance:

- a user can configure original + several target languages without editing config files
- invalid combinations are prevented or explained
- legacy/simple mode remains understandable

### MLC-010 - Performance, stress, and lifecycle hardening

**Purpose:** Verify the feature under real streaming load.

Tests:

- 1 original + 1, 3, and 4 translations
- local-only, cloud-only, and mixed backends
- slow/failing translation provider
- repeated partial captions with full-sentence translation
- output start/stop and filter removal with jobs in flight
- recording start/reset behavior
- OBS text + files + WebVTT + live sink enabled together
- long-duration run for queue growth and memory stability

Measure:

- transcription latency unchanged within expected noise
- added per-language translation latency
- queue depth/backpressure
- CPU/GPU use for local translation
- memory growth
- dropped/late translation count

Acceptance:

- no unbounded queue/thread growth
- no use-after-free/data-race issues found in lifecycle testing
- original-language captions remain available when translations fail
- performance limits are documented

### MLC-011 - Live-platform capability research

**Purpose:** Determine whether true viewer-selectable multilingual live CC can be transported to major services.

Research/verification targets:

- OBS/libobs support for multiple CEA-608/708 caption services
- RTMP/FLV carriage available through current OBS APIs
- YouTube Live ingestion/display behavior
- Twitch ingestion/display behavior
- Facebook Live ingestion/display behavior
- whether WebVTT-in-SEI paths are preserved by each service

Deliverable:

- `docs/LIVE_CAPTION_CAPABILITY_MATRIX.md`
- tested transport/platform matrix with dates and OBS versions
- implementation recommendation for any verified multi-track path

Do not block MLC-001 through MLC-010 on platform support. The rest of the multilingual feature is useful independently.

### MLC-012 - Documentation, release notes, and upstream proposal

**Purpose:** Make the work reviewable upstream and maintainable in the fork.

Changes:

- user-facing configuration documentation
- architecture notes for contributors
- migration notes
- known platform limitations
- performance guidance
- release notes

Upstream proposal should describe the capability as **multilingual caption routing**, not only "multiple RTMP captions". The proposal should emphasize that it generalizes existing features and reuses the current text/file/WebVTT/translation paths.

If upstream declines the feature, continue implementation in the fork while keeping changes modular enough to rebase onto upstream releases.

## 10. Recommended PR order

```text
PR 1  MLC-001                       Core caption model + regression baseline
PR 2  MLC-002                       Config model + legacy migration
PR 3  MLC-003                       Translation dispatcher
PR 4  MLC-004 + MLC-005             Router + OBS text sources
PR 5  MLC-006                       Multi-language files
PR 6  MLC-007                       WebVTT/recording tracks
PR 7  MLC-008 + MLC-009             Live single-track selection + UI
PR 8  MLC-010                       Stress/performance/lifecycle hardening
PR 9  MLC-011                       Live-platform capability matrix
PR 10 MLC-012                       Final docs/release/upstream polish
```

The order intentionally delivers useful multilingual outputs before depending on uncertain third-party live-platform support.

## 11. Definition of done

The feature is complete for the fork when:

- one Whisper transcription can fan out to up to five configured languages
- original and translated captions share stable sequence IDs/timestamps
- translations use managed bounded execution and shut down safely
- each language can independently target an OBS text source
- each language can independently produce TXT/SRT output
- supported outputs can receive multiple WebVTT language tracks
- the traditional live caption output can use any selected generated language
- multiple live caption tracks are used only where transport/platform support has been verified
- current single-language configurations continue to work
- translation/sink failures are isolated
- sustained-load testing shows no unbounded worker/queue/memory growth
- behavior, platform limitations, and migration are documented

## 12. Explicit non-goals for the first implementation

- running separate Whisper instances per language
- unlimited simultaneous language count
- guaranteeing selectable multilingual CC on services that accept only one live caption track
- translating every partial transcription by default
- replacing existing local/cloud translation providers
- redesigning unrelated Whisper/VAD/model-loading code
- requiring a custom OBS UI plugin before basic multilingual settings can ship

## 13. Upstream issue outline

Recommended title:

> Feature proposal: first-class multilingual caption routing and outputs

Key points to include:

- LocalVocal already supports transcription, translation, text sources, subtitle files, live captions, and multi-language WebVTT primitives, but translation targets are mostly single-target today.
- Proposed change: one transcription -> multiple target languages -> independently configurable sinks.
- Initial limit: five languages, matching current WebVTT track capacity.
- Preserve existing settings and single-language behavior.
- Deliver immediate value for multiple OBS text sources, synchronized subtitle files, and recording/WebVTT tracks even when a streaming platform only accepts one live caption track.
- Keep true multi-track livestream captions capability-aware and gated on verified OBS/platform support.
- Ask the maintainer whether this architecture fits upstream direction before submitting the larger implementation series.

The fork should remain the implementation source of truth if upstream does not accept the feature.