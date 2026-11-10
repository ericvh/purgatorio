# mcpd: an MCP session over a pipe. The reply to initialize carries the
# whole sh primer, so it is left out.
{
	echo '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"sh","version":"0"}}}'
	echo '{"jsonrpc":"2.0","method":"notifications/initialized"}'
	echo '{"jsonrpc":"2.0","id":2,"method":"ping"}'
	echo '{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"run","arguments":{"command":"echo hi; ls /; cat /nonexistent"}}}'
	echo '{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"write_file","arguments":{"path":"f","content":"AAEC/w==","encoding":"base64"}}}'
	echo '{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"read_file","arguments":{"path":"/tmp/f"}}}'
	echo '{"jsonrpc":"2.0","id":6,"method":"tools/call","params":{"name":"run","arguments":{"command":"echo x > /bin/x; rm /bin/cat.wasm; bind ''#c'' /n; echo x > /dev/cons; wc -c f","stdin":"unused"}}}'
	echo '{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"reset","arguments":{}}}'
	echo '{"jsonrpc":"2.0","id":8,"method":"tools/call","params":{"name":"list_dir","arguments":{"path":"/tmp"}}}'
	echo '{"jsonrpc":"2.0","id":9,"method":"tools/call","params":{"name":"stat","arguments":{"path":"/tmp/f"}}}'
	echo '{"jsonrpc":"2.0","id":10,"method":"nosuch"}'
	echo 'not json'
} | mcpd | grep -v '"id":1,'
echo status $status
