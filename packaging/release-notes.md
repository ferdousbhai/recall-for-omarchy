Recall for Omarchy v0.7.0 keeps its model in the standard Hugging Face cache, so `hf`, llama.cpp and other local apps can see the weights and there is one copy.

- The default location is `$HF_HUB_CACHE`, `$HF_HOME/hub`, `$XDG_CACHE_HOME/huggingface/hub` or `~/.cache/huggingface/hub`, in a real hub layout at the pinned revision. Files are still SHA-256 verified before they are published, and anything already in that snapshot is checked when the service starts.
- Upgrading moves files from `~/.local/share/recall/Models` into the cache without downloading them again, and your index stays valid. Search keeps working during the move.
- A `models_directory` you set yourself is still honoured. A setting that points at a snapshot of this model now follows the pinned revision, so a future pin can't land in a stale folder. `recall config set models_directory null` returns to the default.
- Uninstalling leaves the model in the cache, because other apps can share it; the package's removal message says how to delete it.
- `recall models download` fetches the files without the service, and `recall models` lists where they are.

Based on pdstn12/Recall (MIT), inspired by ARahim3/DigUp.
