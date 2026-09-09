// A relative cross-package import (rather than the workspace package name)
// so this resolves correctly whether or not `pnpm install` actually linked
// the workspace packages into node_modules -- the point of this fixture is
// a real cross-file reference for scip-typescript to resolve, not a test of
// pnpm's own linking.
import { greet } from "../../pkg-a/src/index";

export function shout(): string {
  return greet().toUpperCase();
}
