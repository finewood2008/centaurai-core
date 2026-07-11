-- Rebrand the internal engine without changing its stable ID or wire type.
UPDATE agent_metadata
SET name = 'CentaurAI Core',
    icon = '/api/assets/logos/brand/centaurai.svg',
    updated_at = CAST(strftime('%s', 'now') AS INTEGER) * 1000
WHERE id = '632f31d2';
