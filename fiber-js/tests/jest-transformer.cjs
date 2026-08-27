const { transformSync } = require("esbuild");

module.exports = {
    process(source, sourcePath) {
        if (/\.ya?ml$/.test(sourcePath)) {
            return {
                code: `module.exports = ${JSON.stringify(source)};`
            };
        }

        return {
            code: transformSync(source, {
                format: "cjs",
                loader: "ts",
                sourcefile: sourcePath,
                sourcemap: "inline",
                target: "node20"
            }).code
        };
    }
};
