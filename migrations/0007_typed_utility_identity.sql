-- An unclassified legacy service needs an explicit classification, never a guess.
CREATE TEMP TABLE utility_service_kind_guard (
    kind TEXT NOT NULL CHECK (kind IN (
        'waste', 'recycling', 'electricity', 'gas', 'water', 'propane', 'internet'
    ))
);
INSERT INTO utility_service_kind_guard(kind)
SELECT json_extract(service.value, '$.kind')
FROM accounts AS account, json_each(account.binding, '$.services') AS service
WHERE account.binding IS NOT NULL;
INSERT INTO utility_service_kind_guard(kind)
SELECT json_extract(payload, '$.kind') FROM observations WHERE kind='service';
INSERT INTO utility_service_kind_guard(kind)
SELECT json_extract(meter.value, '$.kind')
FROM utility_connections AS connection,
     json_each(connection.progress, '$.accounts') AS account,
     json_each(account.value, '$.meters') AS meter;
DROP TABLE utility_service_kind_guard;

ALTER TABLE utility_connections DROP COLUMN name;

UPDATE accounts
SET binding=json_set(
    json_remove(binding, '$.name'),
    '$.services', json((
        SELECT json_group_array(json_remove(service.value, '$.name'))
        FROM json_each(accounts.binding, '$.services') AS service
    ))
)
WHERE binding IS NOT NULL;

UPDATE observations SET payload=json_remove(payload, '$.name') WHERE kind='service';
