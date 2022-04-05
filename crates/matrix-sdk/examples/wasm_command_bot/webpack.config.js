const path = require('path');
const HTMLWebpackPlugin = require('html-webpack-plugin');
const WasmPackPlugin = require("@wasm-tool/wasm-pack-plugin");

module.exports = {
    mode: 'development',
    devtool: 'eval-source-map',
    entry: './index.js',
    output: {
        filename: 'index.js',
        path: path.resolve(__dirname, 'dist'),
    },
    experiments: {
        asyncWebAssembly: true
    },
    resolve: {
        extensions: ['.js'],
    },
    plugins: [
        new HTMLWebpackPlugin(),
        new WasmPackPlugin({
            crateDirectory: path.resolve(__dirname, ".")
        }),
    ],
};
