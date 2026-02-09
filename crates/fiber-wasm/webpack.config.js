const path = require('path');
const fs = require("fs");
const fsp = require("fs/promises");
const WasmPackPlugin = require("@wasm-tool/wasm-pack-plugin");
const webpack = require("webpack");
const isDev = process.env.NODE_ENV === 'development'

class CopyWasmTypesPlugin {
    apply(compiler) {
        compiler.hooks.afterEmit.tapPromise("CopyWasmTypesPlugin", async () => {
            const srcDir = path.resolve(__dirname, "pkg");
            const dstDir = path.resolve(__dirname, "dist", "pkg");
            await fsp.mkdir(dstDir, { recursive: true });
            const files = ["fiber-wasm.d.ts", "fiber-wasm_bg.wasm.d.ts"];
            for (const file of files) {
                const src = path.join(srcDir, file);
                if (fs.existsSync(src)) {
                    await fsp.copyFile(src, path.join(dstDir, file));
                }
            }
        });
    }
}
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
        new CopyWasmTypesPlugin(),
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
