DROP TABLE IF EXISTS outstanding_batches CASCADE;
DROP INDEX CONCURRENTLY IF EXISTS aggregate_share_jobs_interval_containment_index CASCADE;
DROP TABLE IF EXISTS aggregate_share_jobs CASCADE;
DROP INDEX CONCURRENTLY IF EXISTS collection_jobs_interval_containment_index CASCADE;
DROP INDEX CONCURRENTLY IF EXISTS collection_jobs_lease_expiry CASCADE;
DROP TABLE IF EXISTS collection_jobs CASCADE;
DROP TYPE IF EXISTS COLLECTION_JOB_STATE CASCADE;
DROP TABLE IF EXISTS batch_aggregations CASCADE;
DROP INDEX CONCURRENTLY IF EXISTS report_aggregations_client_report_id_index CASCADE;
DROP INDEX CONCURRENTLY IF EXISTS report_aggregations_aggregation_job_id_index CASCADE;
DROP TABLE IF EXISTS report_aggregations CASCADE;
DROP TYPE IF EXISTS REPORT_AGGREGATION_STATE CASCADE;
DROP INDEX CONCURRENTLY IF EXISTS aggregation_jobs_task_and_client_timestamp_interval CASCADE;
DROP INDEX CONCURRENTLY IF EXISTS aggregation_jobs_task_and_batch_id CASCADE;
DROP INDEX CONCURRENTLY IF EXISTS aggregation_jobs_state_and_lease_expiry CASCADE;
DROP TABLE IF EXISTS aggregation_jobs CASCADE;
DROP TYPE IF EXISTS AGGREGATION_JOB_STATE CASCADE;
DROP INDEX CONCURRENTLY IF EXISTS client_reports_task_and_timestamp_index CASCADE;
DROP INDEX CONCURRENTLY IF EXISTS client_reports_task_unaggregated CASCADE;
DROP TABLE IF EXISTS client_reports CASCADE;
DROP TABLE IF EXISTS task_vdaf_verify_keys CASCADE;
DROP TABLE IF EXISTS task_hpke_keys CASCADE;
DROP TABLE IF EXISTS task_collector_auth_tokens CASCADE;
DROP TABLE IF EXISTS task_aggregator_auth_tokens CASCADE;
DROP INDEX CONCURRENTLY IF EXISTS task_id_index CASCADE;
DROP TABLE IF EXISTS tasks CASCADE;
DROP TYPE IF EXISTS AGGREGATOR_ROLE CASCADE;
DROP EXTENSION IF EXISTS btree_gist CASCADE;
DROP EXTENSION IF EXISTS pgcrypto CASCADE;