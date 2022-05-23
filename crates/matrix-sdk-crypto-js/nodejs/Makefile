build:
	cd .. && napi build --platform --release --features nodejs
	test -f ../index.js && mv ../index.js pkg/ || true
	test -f ../index.d.ts && mv ../index.d.ts pkg/ || true
	test -f ../matrix-sdk-crypto.*.node && mv ../matrix-sdk-crypto.*.node pkg/ || true
	echo '*' > pkg/.gitignore

test:
	echo 'nop'
