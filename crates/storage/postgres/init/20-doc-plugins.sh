#!/bin/bash
# Creates the database plugins' collections live in (T62) and the role the backend keeps them with.
# Safe to run again: the storage stack's setup service does, for databases made before T62.
set -euo pipefail

psql -v ON_ERROR_STOP=1 --username "$POSTGRES_USER" --dbname postgres \
  -v data="$DOC_DATA_USER" -v data_password="$DOC_DATA_PASSWORD" -v db="$DOC_PLUGINS_DB" <<-'EOSQL'
	SELECT format('CREATE ROLE %I LOGIN', :'data')
	WHERE NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = :'data') \gexec
	SELECT format('ALTER ROLE %I LOGIN PASSWORD %L', :'data', :'data_password') \gexec
	SELECT format('CREATE DATABASE %I OWNER %I', :'db', :'data')
	WHERE NOT EXISTS (SELECT 1 FROM pg_database WHERE datname = :'db') \gexec
	SELECT format('REVOKE ALL ON DATABASE %I FROM PUBLIC', :'db') \gexec
EOSQL
