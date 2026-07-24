#!/usr/bin/env node
"use strict";

const { spawnSync } = require("node:child_process");

const PACKAGES = {
  linux: {
    x64: "clooterminal-linux-x64",
  },
};

const packageName = PACKAGES[process.platform]?.[process.arch];
if (packageName === undefined) {
  console.error(`cloo does not support ${process.platform}-${process.arch}.`);
  process.exitCode = 1;
} else {
  let binary;
  try {
    binary = require.resolve(`${packageName}/bin/cloo`);
  } catch {
    console.error(
      `cloo could not find its optional ${packageName} binary package. Reinstall clooterminal.`,
    );
    process.exitCode = 1;
  }

  if (binary !== undefined) {
    const result = spawnSync(binary, process.argv.slice(2), { stdio: "inherit" });
    if (result.error !== undefined) {
      console.error(`cloo could not start: ${result.error.message}`);
      process.exitCode = 1;
    } else {
      process.exitCode = result.status ?? 1;
    }
  }
}
