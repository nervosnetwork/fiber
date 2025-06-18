const path = require('path');
const WasmPackPlugin = require("@wasm-tool/wasm-pack-plugin");
const webpack = require("webpack");
const isDev = process.env.NODE_ENV === 'development'
module.exports = {
    entry: './index.ts',
    target: "es2017",
    output: {
        path: path.resolve(__dirname, 'dist'),
        filename: 'index.js',
        library: {
            type: "module",
        },
        publicPath: "/",
        chunkFormat: false
    },
    plugins: [
        new WasmPackPlugin({
            crateDirectory: path.resolve(__dirname, "."),
            outName: "fiber-wasm",
            extraArgs: "--target web" + (isDev ? " --dev" : "")
        }),
        new webpack.ProvidePlugin({
            Buffer: ['buffer', 'Buffer'],
        }),
    ],
    module: {
        rules: [
            {
                test: /\.wasm$/,
                loader: "arraybuffer-loader"
            },
            {
                test: /\.ts$/,
                use: 'ts-loader',
                exclude: /node_modules/,
            }
        ]

    },
    experiments: {
        outputModule: true
    },
    mode: "production",
    resolve: {
        fallback: {
            buffer: require.resolve('buffer/'),
        },
    },
};
