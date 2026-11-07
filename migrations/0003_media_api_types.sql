-- Media API types: virtual models (and direct models) may serve the wider
-- OpenAI surface — image generation, audio speech/transcription/translation
-- and video passthrough — not just text endpoints.
ALTER TABLE virtual_models
    DROP CONSTRAINT virtual_models_api_type_check,
    ADD CONSTRAINT virtual_models_api_type_check
        CHECK (api_type IN (
            'chat','completions','embeddings','multi',
            'images','audio_speech','audio_transcription','audio_translation','video'
        ));
