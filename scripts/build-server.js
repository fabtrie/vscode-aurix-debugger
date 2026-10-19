// Builds the aurix-dbg debug server (release) so the extension can package it.
//
// Jim Tcl is compiled with MinGW gcc, which needs its DLLs on PATH. The MinGW
// bin folder is taken from the environment variable MINGW_BIN, default
// C:\msys64\mingw64\bin.
const { spawnSync } = require('child_process');
const os = require('os');
const path = require('path');

const serverDir = path.resolve(__dirname, '..', 'aurix-debugger', 'server');
const mingwBin = process.env.MINGW_BIN || 'C:\\msys64\\mingw64\\bin';
const cargoBin = path.join(os.homedir(), '.cargo', 'bin');

const env = { ...process.env };
const pathKey = Object.keys(env).find((k) => k.toUpperCase() === 'PATH') || 'PATH';
env[pathKey] = [mingwBin, cargoBin, env[pathKey]].join(path.delimiter);

const result = spawnSync('cargo', ['build', '--release', '-p', 'aurix-dbg'], {
	cwd: serverDir,
	env,
	stdio: 'inherit',
	shell: process.platform === 'win32',
});
process.exit(result.status ?? 1);
