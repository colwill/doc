#!/bin/bash
# Creates the owner role (migrations) and the application role (runtime).
set -euo pipefail

psql -v ON_ERROR_STOP=1 \
  --username "$POSTGRES_USER" --dbname "$DOC_DB" \
  -v db="$DOC_DB" \
  -v owner="$DOC_OWNER_USER" -v owner_password="$DOC_OWNER_PASSWORD" \
  -v app="$DOC_APP_USER" -v app_password="$DOC_APP_PASSWORD" <<-'EOSQL'
	CREATE ROLE :"owner" LOGIN PASSWORD :'owner_password';
	CREATE ROLE :"app" LOGIN PASSWORD :'app_password';

	ALTER DATABASE :"db" OWNER TO :"owner";
	REVOKE ALL ON DATABASE :"db" FROM PUBLIC;
	GRANT CONNECT, TEMPORARY ON DATABASE :"db" TO :"owner", :"app";

	REVOKE ALL ON SCHEMA public FROM PUBLIC;
	CREATE SCHEMA core AUTHORIZATION :"owner";
	CREATE SCHEMA core_v1 AUTHORIZATION :"owner";
	GRANT USAGE ON SCHEMA core TO :"app";
	GRANT USAGE ON SCHEMA core_v1 TO :"app";

	ALTER DEFAULT PRIVILEGES FOR ROLE :"owner" IN SCHEMA core
	  GRANT SELECT, INSERT, UPDATE, DELETE ON TABLES TO :"app";
	ALTER DEFAULT PRIVILEGES FOR ROLE :"owner" IN SCHEMA core
	  GRANT USAGE, SELECT ON SEQUENCES TO :"app";
	ALTER DEFAULT PRIVILEGES FOR ROLE :"owner" IN SCHEMA core_v1
	  GRANT SELECT ON TABLES TO :"app";
EOSQL
