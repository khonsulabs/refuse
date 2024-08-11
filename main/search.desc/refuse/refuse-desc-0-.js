searchState.loadedDescShard("refuse", 0, "Refuse\nA type-erased garbage collected reference.\nA type-erased root garbage collected reference.\nA type that can be garbage collected.\nA guard that prevents garbage collection while held.\nA type that can be garbage collected that cannot contain …\nA pool of garbage collected values.\nIf true, this type may contain references and should have …\nA mapping from one type to another.\nDerives the <code>refuse::MapAs</code> trait for a given struct or enum.\nA type that implements <code>MapAs</code> with an empty implementation.\nA reference to data stored in a garbage collector.\nA root reference to a <code>T</code> that has been allocated in the …\nA type that can contain no <code>Ref&lt;T&gt;</code>s and has an empty <code>MapAs</code> …\nThe target type of the mapping.\nA type that can find and mark any references it has.\nDerives the <code>refuse::Trace</code> trait for a given struct or enum.\nA tracer for the garbage collector.\nAn error indicating an operation would deadlock.\nA marker indicating that a coordinated yield has completed.\nA pending yield to the garbage collector.\nAcquires a lock that prevents the garbage collector from …\nReturns a guard that allocates from <code>pool</code>.\nArchitecture overview of the underlying design of Refuse.\nReturns this reference as an untyped reference.\nReturns an untyped “weak” reference to this root.\nLoads a root reference to the underlying data. Returns <code>None</code>…\nInvokes the garbage collector.\nManually invokes the garbage collector.\nPerform a coordinated yield to the collector, if needed.\nReturns a <code>Ref&lt;T&gt;</code>, if <code>T</code> matches the type of this reference.\nReturns a <code>Ref&lt;T&gt;</code>, if <code>T</code> matches the type of this reference.\nReturns a <code>Ref&lt;T&gt;</code>.\nReturns a <code>Ref&lt;T&gt;</code>.\nReturns a <code>Root&lt;T&gt;</code> if the underlying reference points to a <code>T</code>…\nReturns a <code>Root&lt;T&gt;</code> if the underlying reference points to a <code>T</code>…\nReturns a “weak” reference to this root.\nReturns an untyped “weak” reference erased to this …\nAcquires a collection guard for this pool.\nReturns the argument unchanged.\nReturns the argument unchanged.\nReturns the argument unchanged.\nReturns the argument unchanged.\nReturns the argument unchanged.\nReturns the argument unchanged.\nReturns the argument unchanged.\nReturns the argument unchanged.\nReturns the argument unchanged.\nReturns the argument unchanged.\nCalls <code>U::from(self)</code>.\nCalls <code>U::from(self)</code>.\nCalls <code>U::from(self)</code>.\nCalls <code>U::from(self)</code>.\nCalls <code>U::from(self)</code>.\nCalls <code>U::from(self)</code>.\nCalls <code>U::from(self)</code>.\nCalls <code>U::from(self)</code>.\nCalls <code>U::from(self)</code>.\nCalls <code>U::from(self)</code>.\nReturns this root as an untyped root.\nLoads a reference to the underlying data. Returns <code>None</code> if …\nLoads a reference to the underlying data. Returns <code>None</code> if <code>T</code>…\nLoads a reference to the underlying data. Returns <code>None</code> if …\nReturns a reference to the result of <code>MapAs::map_as()</code>, if …\nMaps <code>self</code> to target type.\nMarks <code>collectable</code> as being referenced, ensuring it is not …\nStores <code>value</code> in the garbage collector, returning a root …\nStores <code>value</code> in the garbage collector, returning a “weak…\nReturns true if these two references point to the same …\nReturns the current number of root references to this …\nReturns an untyped root reference.\nTraces all refrences that this value references.\nTries to acquire a lock that prevents the garbage …\nInvokes the garbage collector.\nManually invokes the garbage collector.\nTry to convert a typeless root reference into a <code>Root&lt;T&gt;</code>.\nExecutes <code>unlocked</code> while this guard is temporarily released.\nReturns a root for this reference, if the value has not …\nWaits for the garbage collector to finish the current …\nExecutes <code>unlocked</code> while this guard is temporarily released.\nYield to the garbage collector, if needed.")