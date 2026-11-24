(ns refdata.dataset
  (:import [java.io File]))

(def num-securities 2000)
(def num-read-warmup 500)
(def num-read-samples 2000)

(def exchanges ["XNAS" "XNYS" "XLON" "XTKS" "XHKG"])
(def currencies ["USD" "USD" "GBP" "JPY" "HKD"])

(defn make-securities
  "Generate deterministic Trading Reference Data Security Master records."
  [n]
  (let [base (vec
              (for [i (range n)]
                (let [ex-idx (mod i (count exchanges))]
                  {:sec/id           (format "SEC-%05d" i)
                   :sec/ticker       (if (zero? i) "AAPL" (format "T%05d" i))
                   :sec/isin         (if (zero? i) "US0378331005" (format "US%010d" (+ 1000000000 i)))
                   :sec/cusip        (if (zero? i) "037833100" (format "%09d" (+ 100000000 i)))
                   :sec/name         (if (zero? i)
                                       "Apple Inc."
                                       (format "Global Quant Holdings %05d Corp" i))
                   :sec/exchange     (nth exchanges ex-idx)
                   :sec/currency     (nth currencies ex-idx)
                   :sec/lot-size     (long (if (zero? (mod i 10)) 100 1))
                   :sec/tick-size    0.01
                   :sec/split-factor 1.0
                   :sec/status       "ACTIVE"})))]
    base))

(defn temp-dir
  "Return a unique temporary directory path (not yet created)."
  [prefix]
  (let [dir (File. (str "/tmp/" prefix "-" (java.util.UUID/randomUUID)))]
    (.getAbsolutePath dir)))

(defn delete-recursively!
  [path]
  (let [f (File. ^String path)]
    (when (.exists f)
      (doseq [child (reverse (file-seq f))]
        (.delete ^File child)))))

(defn now-ns [] (System/nanoTime))

(defn percentile
  [sorted-vals p]
  (let [cnt (count sorted-vals)
        idx (min (dec cnt) (max 0 (int (Math/floor (* p cnt)))))]
    (nth sorted-vals idx)))

(defn bench-reads
  "Warm up and measure point-read latency in microseconds across n-samples."
  [tickers lookup-fn]
  (let [n-tickers (count tickers)]
    (dotimes [i num-read-warmup]
      (lookup-fn (nth tickers (mod i n-tickers))))
    (let [latencies-ns
          (vec
           (for [i (range num-read-samples)]
             (let [t (nth tickers (mod i n-tickers))
                   t0 (now-ns)
                   res (lookup-fn t)
                   t1 (now-ns)]
               (when (nil? res)
                 (throw (ex-info "Lookup returned nil during benchmark" {:ticker t})))
               (- t1 t0))))
          sorted-us (vec (sort (map #(/ (double %) 1000.0) latencies-ns)))
          mean-us (/ (reduce + 0.0 sorted-us) (count sorted-us))]
      {:p50-us  (percentile sorted-us 0.50)
       :p95-us  (percentile sorted-us 0.95)
       :p99-us  (percentile sorted-us 0.99)
       :mean-us mean-us})))

(defonce results (atom {}))

(defn record-result! [engine k v]
  (swap! results assoc-in [engine k] v))
