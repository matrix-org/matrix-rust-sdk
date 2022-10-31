const { Agent } = require('https');
const { DownloaderHelper } = require('node-downloader-helper');
const { version } = require("./package.json");
const { platform, arch } = process

const DOWNLOADS_BASE_URL = "https://github.com/matrix-org/matrix-rust-sdk/releases/download";
const CURRENT_VERSION = `matrix-sdk-crypto-nodejs-v${version}`;

const byteHelper = function (value) {
    if (value === 0) {
        return '0 b';
    }
    const units = ['b', 'kB', 'MB', 'GB', 'TB'];
    const number = Math.floor(Math.log(value) / Math.log(1024));
    return (value / Math.pow(1024, Math.floor(number))).toFixed(1) + ' ' +
        units[number];
};

function download_lib(libname) {
    let startTime = new Date();

    const url = `${DOWNLOADS_BASE_URL}/${CURRENT_VERSION}/${libname}`;
    console.info(`Downloading lib ${libname} from ${url}`);
    const dl = new DownloaderHelper(url, __dirname, {
        override: true,
        httpsRequestOptions: {
          // Disable keepalive to prevent the process hanging open.
          // https://github.com/matrix-org/matrix-rust-sdk/issues/1160
          agent: new Agent({ keepAlive: false }),
        },
    });

    dl.on('end', () => console.info('Download Completed'));
    dl.on('error', (err) => console.info('Download Failed', err));
    dl.on('progress', stats => {
        const progress = stats.progress.toFixed(1);
        const speed = byteHelper(stats.speed);
        const downloaded = byteHelper(stats.downloaded);
        const total = byteHelper(stats.total);

        // print every one second (`progress.throttled` can be used instead)
        const currentTime = new Date();
        const elaspsedTime = currentTime - startTime;
        if (elaspsedTime > 1000) {
            startTime = currentTime;
            console.info(`${speed}/s - ${progress}% [${downloaded}/${total}]`);
        }
    });
    dl.start().catch(err => console.error(err));
}

function isMusl() {
  // For Node 10
  if (!process.report || typeof process.report.getReport !== 'function') {
    try {
      return readFileSync('/usr/bin/ldd', 'utf8').includes('musl')
    } catch (e) {
      return true
    }
  } else {
    const { glibcVersionRuntime } = process.report.getReport().header
    return !glibcVersionRuntime
  }
}

switch (platform) {
  case 'win32':
    switch (arch) {
      case 'x64':
        download_lib('matrix-sdk-crypto.win32-x64-msvc.node')
        break
      case 'ia32':
        download_lib('matrix-sdk-crypto.win32-ia32-msvc.node')
        break
      case 'arm64':
        download_lib('matrix-sdk-crypto.win32-arm64-msvc.node')
        break
      default:
        throw new Error(`Unsupported architecture on Windows: ${arch}`)
    }
    break
  case 'darwin':
    switch (arch) {
      case 'x64':
        download_lib('matrix-sdk-crypto.darwin-x64.node')
        break
      case 'arm64':
        download_lib('matrix-sdk-crypto.darwin-arm64.node')
        break
      default:
        throw new Error(`Unsupported architecture on macOS: ${arch}`)
    }
    break
  case 'linux':
    switch (arch) {
      case 'x64':
        if (isMusl()) {
          download_lib('matrix-sdk-crypto.linux-x64-musl.node')
        } else {
          download_lib('matrix-sdk-crypto.linux-x64-gnu.node')
        }
        break
      case 'arm64':
        if (isMusl()) {
            throw new Error('Linux for arm64 musl isn\'t support at the moment')
        } else {
          download_lib('matrix-sdk-crypto.linux-arm64-gnu.node')
        }
        break
      case 'arm':
        download_lib('matrix-sdk-crypto.linux-arm-gnueabihf.node')
        break
      default:
        throw new Error(`Unsupported architecture on Linux: ${arch}`)
    }
    break
  default:
    throw new Error(`Unsupported OS: ${platform}, architecture: ${arch}`)
}
