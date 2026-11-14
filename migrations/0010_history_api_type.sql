-- requests_history.api_type: endpoint family that produced the row
-- ('chat','completions','embeddings','images','audio_*','video'). Omni
-- virtual models (api_type='multi') make one api_name serve several
-- families — without this column their history rows are indistinguishable.
ALTER TABLE requests_history
    ADD COLUMN api_type TEXT NOT NULL DEFAULT 'chat';
COMMENT ON COLUMN requests_history.api_type IS
    'Endpoint family of the request (chat|completions|embeddings|images|audio_speech|audio_transcription|audio_translation|video) — recorded per row so omni/multi virtual-model traffic stays attributable.';
