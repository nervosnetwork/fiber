module.exports = {
    moduleFileExtensions: ["ts", "js", "cjs", "json", "yml", "yaml"],
    testEnvironment: "node",
    transform: {
        "^.+\\.(ts|ya?ml)$": "<rootDir>/tests/jest-transformer.cjs"
    }
};
