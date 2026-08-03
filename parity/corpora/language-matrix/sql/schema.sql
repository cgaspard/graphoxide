CREATE TABLE matrix_worker (
    id integer PRIMARY KEY,
    value text NOT NULL
);

CREATE TABLE matrix_job (
    id integer PRIMARY KEY,
    worker_id integer REFERENCES matrix_worker(id)
);

CREATE VIEW pending_jobs AS
SELECT matrix_job.id
FROM matrix_job
JOIN matrix_worker ON matrix_worker.id = matrix_job.worker_id;

CREATE FUNCTION process_worker() RETURNS integer AS $$
    SELECT count(*) FROM matrix_worker;
$$ LANGUAGE SQL;
