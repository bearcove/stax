import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

const workspaceSource = (path: string) => new URL(path, import.meta.url).pathname;

export default defineConfig({
  plugins: [react()],
  resolve: {
    alias: {
      "@bearcove/phon-engine": workspaceSource("../../oss/facet/phon/typescript/packages/phon-engine/src/index.ts"),
      "@bearcove/phon-schema": workspaceSource("../../oss/facet/phon/typescript/packages/phon-schema/src/index.ts"),
      "@bearcove/vox-core": workspaceSource("../../oss/facet/vox/typescript/packages/vox-core/src/index.ts"),
      "@bearcove/vox-tcp": workspaceSource("../../oss/facet/vox/typescript/packages/vox-tcp/src/index.ts"),
      "@bearcove/vox-wire": workspaceSource("../../oss/facet/vox/typescript/packages/vox-wire/src/index.ts"),
      "@bearcove/vox-ws": workspaceSource("../../oss/facet/vox/typescript/packages/vox-ws/src/index.ts"),
    },
  },
  server: {
    port: 5173,
  },
});
