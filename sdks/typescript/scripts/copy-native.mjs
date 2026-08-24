import { copyFile, mkdir } from "node:fs/promises";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const packageRoot = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const source = resolve(packageRoot, "../../target/release/libmars_sdk_node.dylib");
const destination = resolve(packageRoot, "native/mars_sdk_node.node");
await mkdir(dirname(destination), { recursive: true });
await copyFile(source, destination);
