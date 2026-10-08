
			target="{{ .Target }}"

			# Exit early if target already exists
			if [ -d "$target" ]; then
			    exit 0
			fi

			# Find the first non-existent parent
			first_new=""
			check="$target"
			while [ ! -d "$check" ]; do
			    first_new="$check"
			    check=$(dirname "$check")
			done

			# Create and chown from the top (only if we found new directories to create)
			if [ -n "$first_new" ]; then
			    mkdir -p "$target" && chown -R {{ .User }}:{{ .User }} "$first_new"
			fi
    