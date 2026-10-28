-- ============================================================================
-- 0002_auth_models：认证 profile 的模型目录缓存
-- /validate 返回的模型列表以 JSON 存 auth_profiles.models_json，供 /v1/models
-- 与诊断页面复用；不改变既有列语义，可安全追加。
-- ============================================================================

ALTER TABLE auth_profiles ADD COLUMN models_json TEXT;
