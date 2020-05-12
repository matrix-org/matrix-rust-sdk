const rust = import('./pkg');

rust
    .then(m => {
        return m.run()
    })
    .catch(console.error);
