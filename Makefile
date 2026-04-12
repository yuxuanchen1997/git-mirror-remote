.PHONY: build up down restart logsclean

build:
	podman build -t git-cache-proxy .

up:
	podman compose up -d

down:
	podman compose down

restart: down up

logs:
	podman compose logs -f

clean:
	podman compose down
	# Note: volume persists. Run 'make purge' to delete cache.

purge:
	podman compose down
	rm -rf cache