download_postgres_binary:
	wget 'https://github.com/theseus-rs/postgresql-binaries/releases/download/17.4.0/postgresql-17.4.0-aarch64-apple-darwin.tar.gz'
	#  xattr -d com.apple.quarantine postgres-17/lib/*
	#  xattr -d com.apple.quarantine postgres-17/bin/*
	# extract
	# run ./run-postgres.sh