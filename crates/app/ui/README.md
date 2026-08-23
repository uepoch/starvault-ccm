# StarVault CCM UI

React frontend for the StarVault CCM Tauri application. Campaign management
lives in the Library; filesystem transitions and recovery remain in
`svccm-core` and are exposed through small Tauri command adapters.

From this directory:

```sh
vp install
vp dev
vp check
vp test
vp build
```

To run or build the desktop shell, use
`node_modules/.bin/tauri dev` or `node_modules/.bin/tauri build`.
