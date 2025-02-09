# vmkit-core

vmkit-core is a library which provides building blocks for building a language runtime.

# Features
- Garbage Collector: Done by using [mmtk-core](https://github.com/mmtk/mmtk-core)
- Thread management: Threads can be spawned, suspended, resumed, killed.


## vmkit-core flavors

We support three flavors of vmkit-core:
- `uncooperative`: This is the most conservative flavor. It exposes a BDWGC-like API, only provides Immix and MarkSweep GC plans,
and management of threads is fully internal to the library.
- `cooperative`: This is mostly-precise flavor. This flavor exposes advanced allocation API, requries runtime to call into `yieldpoint()`,
and also requires runtime to do write barriers and have precise object layout. Stack is left to be conservatively scanned. 
- `full-precise`: This is precise flavor. Provides everything that `cooperative` provides, and also requires
runtime to not have any conservative assumptions such as conservative stack scanning.