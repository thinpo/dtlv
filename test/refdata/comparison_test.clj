(ns refdata.comparison-test
  (:require [clojure.test :refer [deftest is testing use-fixtures]]
            [refdata.dataset :as data]
            [datalevin.core :as dl]
            [datahike.api :as dh]
            [xtdb.api :as xt]
            [xtdb.node :as xtn]
            [datomic.client.api :as datomic]
            [datascript.core :as ds])
  (:import [java.time Instant]))

(def datalog-schema
  {:sec/id           {:db/valueType :db.type/string :db/cardinality :db.cardinality/one :db/unique :db.unique/identity}
   :sec/ticker       {:db/valueType :db.type/string :db/cardinality :db.cardinality/one :db/unique :db.unique/identity}
   :sec/isin         {:db/valueType :db.type/string :db/cardinality :db.cardinality/one :db/index true}
   :sec/cusip        {:db/valueType :db.type/string :db/cardinality :db.cardinality/one}
   :sec/name         {:db/valueType :db.type/string :db/cardinality :db.cardinality/one :db/fulltext true}
   :sec/exchange     {:db/valueType :db.type/string :db/cardinality :db.cardinality/one}
   :sec/currency     {:db/valueType :db.type/string :db/cardinality :db.cardinality/one}
   :sec/lot-size     {:db/valueType :db.type/long   :db/cardinality :db.cardinality/one}
   :sec/tick-size    {:db/valueType :db.type/double :db/cardinality :db.cardinality/one}
   :sec/split-factor {:db/valueType :db.type/double :db/cardinality :db.cardinality/one}
   :sec/status       {:db/valueType :db.type/string :db/cardinality :db.cardinality/one}})

(def datomic-schema
  [{:db/ident :sec/id           :db/valueType :db.type/string :db/cardinality :db.cardinality/one :db/unique :db.unique/identity}
   {:db/ident :sec/ticker       :db/valueType :db.type/string :db/cardinality :db.cardinality/one :db/unique :db.unique/identity}
   {:db/ident :sec/isin         :db/valueType :db.type/string :db/cardinality :db.cardinality/one}
   {:db/ident :sec/cusip        :db/valueType :db.type/string :db/cardinality :db.cardinality/one}
   {:db/ident :sec/name         :db/valueType :db.type/string :db/cardinality :db.cardinality/one}
   {:db/ident :sec/exchange     :db/valueType :db.type/string :db/cardinality :db.cardinality/one}
   {:db/ident :sec/currency     :db/valueType :db.type/string :db/cardinality :db.cardinality/one}
   {:db/ident :sec/lot-size     :db/valueType :db.type/long   :db/cardinality :db.cardinality/one}
   {:db/ident :sec/tick-size    :db/valueType :db.type/double :db/cardinality :db.cardinality/one}
   {:db/ident :sec/split-factor :db/valueType :db.type/double :db/cardinality :db.cardinality/one}
   {:db/ident :sec/status       :db/valueType :db.type/string :db/cardinality :db.cardinality/one}])

(def datahike-schema
  (mapv (fn [attr]
          (if (= (:db/ident attr) :sec/isin)
            (dissoc attr :db/index)
            (assoc attr :db/index true)))
        datomic-schema))

(def datascript-schema
  {:sec/id     {:db/unique :db.unique/identity}
   :sec/ticker {:db/unique :db.unique/identity}
   :sec/isin   {:db/index true}})

(def securities (data/make-securities data/num-securities))
(def sample-tickers (mapv :sec/ticker (take 200 securities)))

(defn print-summary []
  (println "\n================ TRADING REFERENCE DATA BENCHMARK SUMMARY ================")
  (println (format "%-14s | %10s | %10s | %8s | %8s | %8s | %-24s | %-10s"
                   "Engine" "SOD (ms)" "Rec/sec" "p50 (us)" "p95 (us)" "p99 (us)" "Bitemporality" "Durable"))
  (println (apply str (repeat 110 "-")))
  (doseq [eng [:datalevin :datahike :xtdb-v2 :datomic-local :datascript]]
    (when-let [r (get @data/results eng)]
      (println (format "%-14s | %10.1f | %10.0f | %8.1f | %8.1f | %8.1f | %-24s | %-10s"
                       (name eng)
                       (double (:ingest-ms r 0.0))
                       (double (:ingest-rate r 0.0))
                       (double (get-in r [:reads :p50-us] 0.0))
                       (double (get-in r [:reads :p95-us] 0.0))
                       (double (get-in r [:reads :p99-us] 0.0))
                       (:bitemporal r "N/A")
                       (str (:durable r false))))))
  (println "==========================================================================\n"))

(use-fixtures :once
  (fn [f]
    (f)
    (print-summary)))

;; -----------------------------------------------------------------------------
;; 1. Datalevin: Embedded LMDB, microsecond reads, fulltext, explicit bitemporal
;; -----------------------------------------------------------------------------
(deftest datalevin-refdata-test
  (let [dir (data/temp-dir "dlv-refdata")
        bitemp-schema (merge datalog-schema
                             {:sec-ver/sec-id     {:db/valueType :db.type/string :db/cardinality :db.cardinality/one :db/index true}
                              :sec-ver/valid-from {:db/valueType :db.type/instant :db/cardinality :db.cardinality/one}
                              :sec-ver/valid-to   {:db/valueType :db.type/instant :db/cardinality :db.cardinality/one}
                              :sec-ver/tx-from    {:db/valueType :db.type/instant :db/cardinality :db.cardinality/one}
                              :sec-ver/tx-to      {:db/valueType :db.type/instant :db/cardinality :db.cardinality/one}
                              :sec-ver/split-factor {:db/valueType :db.type/double :db/cardinality :db.cardinality/one}})]
    (try
      (let [conn (dl/get-conn dir bitemp-schema)]
        (testing "SOD batch ingestion"
          (let [t0 (data/now-ns)
                _  (dl/transact! conn securities)
                ms (/ (- (data/now-ns) t0) 1e6)
                cnt (dl/q '[:find (count ?e) . :where [?e :sec/ticker _]] @conn)]
            (is (= data/num-securities cnt))
            (data/record-result! :datalevin :ingest-ms ms)
            (data/record-result! :datalevin :ingest-rate (/ data/num-securities (/ ms 1000.0)))))

        (testing "Intraday point-read latency (LMDB)"
          (let [stats (data/bench-reads
                       sample-tickers
                       (fn [ticker]
                         (dl/q '[:find [?isin ?lot ?sf]
                                 :in $ ?t
                                 :where
                                 [?e :sec/ticker ?t]
                                 [?e :sec/isin ?isin]
                                 [?e :sec/lot-size ?lot]
                                 [?e :sec/split-factor ?sf]]
                               @conn ticker)))]
            (data/record-result! :datalevin :reads stats)
            (is (< (:p95-us stats) 5000.0) "Embedded LMDB reads should be sub-millisecond/low-ms")))

        (testing "Built-in full-text search for trader UI fuzzy lookup"
          (let [hits (dl/q '[:find ?t ?name
                             :in $ ?q
                             :where
                             [(fulltext $ ?q) [[?e _ ?name]]]
                             [?e :sec/ticker ?t]]
                           @conn "Apple")]
            (is (contains? hits ["AAPL" "Apple Inc."]))))

        (testing "Mutable default vs explicit bitemporal version modeling"
          ;; In-place update overwrites previous value (no built-in d/as-of)
          (dl/transact! conn [{:sec/ticker "AAPL" :sec/split-factor 2.0}])
          (is (= 2.0 (dl/q '[:find ?sf . :where [?e :sec/ticker "AAPL"] [?e :sec/split-factor ?sf]] @conn)))
          ;; Explicit bitemporal version entities
          (let [v-eff (java.util.Date/from (Instant/parse "2026-10-01T00:00:00Z"))
                v-end (java.util.Date/from (Instant/parse "9999-12-31T00:00:00Z"))
                t1    (java.util.Date/from (Instant/parse "2026-10-02T09:00:00Z"))
                t2    (java.util.Date/from (Instant/parse "2026-10-03T09:00:00Z"))
                _     (dl/transact! conn [{:db/id "v1"
                                           :sec-ver/sec-id "SEC-00000"
                                           :sec-ver/valid-from v-eff
                                           :sec-ver/valid-to v-end
                                           :sec-ver/tx-from t1
                                           :sec-ver/tx-to v-end
                                           :sec-ver/split-factor 2.0}])
                ;; Correction arrives at T2: close v1 tx-to=T2, insert v2 tx-from=T2
                v1-eid (dl/q '[:find ?e . :where [?e :sec-ver/sec-id "SEC-00000"] [?e :sec-ver/split-factor 2.0]] @conn)
                _     (dl/transact! conn [{:db/id v1-eid :sec-ver/tx-to t2}
                                          {:sec-ver/sec-id "SEC-00000"
                                           :sec-ver/valid-from v-eff
                                           :sec-ver/valid-to v-end
                                           :sec-ver/tx-from t2
                                           :sec-ver/tx-to v-end
                                           :sec-ver/split-factor 4.0}])
                q-bitemp (fn [valid-at tx-at]
                           (dl/q '[:find ?sf .
                                   :in $ ?sid ?vat ?tat
                                   :where
                                   [?e :sec-ver/sec-id ?sid]
                                   [?e :sec-ver/valid-from ?vf]
                                   [?e :sec-ver/valid-to ?vt]
                                   [?e :sec-ver/tx-from ?tf]
                                   [?e :sec-ver/tx-to ?tt]
                                   [(<= (.getTime ^java.util.Date ?vf) (.getTime ^java.util.Date ?vat))]
                                   [(> (.getTime ^java.util.Date ?vt) (.getTime ^java.util.Date ?vat))]
                                   [(<= (.getTime ^java.util.Date ?tf) (.getTime ^java.util.Date ?tat))]
                                   [(> (.getTime ^java.util.Date ?tt) (.getTime ^java.util.Date ?tat))]
                                   [?e :sec-ver/split-factor ?sf]]
                                 @conn "SEC-00000" valid-at tx-at))]
            (is (= 2.0 (q-bitemp (java.util.Date/from (Instant/parse "2026-10-01T12:00:00Z"))
                                 (java.util.Date/from (Instant/parse "2026-10-02T12:00:00Z")))))
            (is (= 4.0 (q-bitemp (java.util.Date/from (Instant/parse "2026-10-01T12:00:00Z"))
                                 (java.util.Date/from (Instant/parse "2026-10-04T12:00:00Z")))))
            (data/record-result! :datalevin :bitemporal "Manual version entities")))

        (dl/close conn)

        (testing "Disk durability across reopen"
          (let [conn2 (dl/get-conn dir bitemp-schema)
                cnt2 (dl/q '[:find (count ?e) . :where [?e :sec/ticker _]] @conn2)]
            (is (= data/num-securities cnt2))
            (data/record-result! :datalevin :durable true)
            (dl/close conn2))))
      (finally
        (data/delete-recursively! dir)))))

;; -----------------------------------------------------------------------------
;; 2. Datahike: Pure-Clojure Datomic semantics, d/history, d/as-of, branching
;; -----------------------------------------------------------------------------
(deftest datahike-refdata-test
  (let [dir (data/temp-dir "dh-refdata")
        cfg {:store {:backend :file :path dir :id (java.util.UUID/randomUUID)}
             :schema-flexibility :write
             :keep-history? true}]
    (try
      (dh/create-database cfg)
      (let [conn (dh/connect cfg)]
        (dh/transact conn datahike-schema)

        (testing "SOD batch ingestion"
          (let [t0 (data/now-ns)
                _  (dh/transact conn securities)
                ms (/ (- (data/now-ns) t0) 1e6)
                cnt (dh/q '[:find (count ?e) . :where [?e :sec/ticker _]] @conn)]
            (is (= data/num-securities cnt))
            (data/record-result! :datahike :ingest-ms ms)
            (data/record-result! :datahike :ingest-rate (/ data/num-securities (/ ms 1000.0)))))

        (testing "Intraday point-read latency"
          (let [stats (data/bench-reads
                       sample-tickers
                       (fn [ticker]
                         (dh/q '[:find [?isin ?lot ?sf]
                                 :in $ ?t
                                 :where
                                 [?e :sec/ticker ?t]
                                 [?e :sec/isin ?isin]
                                 [?e :sec/lot-size ?lot]
                                 [?e :sec/split-factor ?sf]]
                               @conn ticker)))]
            (data/record-result! :datahike :reads stats)))

        (testing "Built-in transaction-time history (d/as-of & d/history)"
          (let [tx1 (dh/transact conn [{:sec/ticker "AAPL" :sec/split-factor 2.0}])
                db-at-t1 (:db-after tx1)
                _   (Thread/sleep 10)
                t1-inst (java.util.Date.)
                _   (Thread/sleep 10)
                _   (dh/transact conn [{:sec/ticker "AAPL" :sec/split-factor 4.0}])]
            ;; Current DB has 4.0
            (is (= 4.0 (dh/q '[:find ?sf . :where [?e :sec/ticker "AAPL"] [?e :sec/split-factor ?sf]] @conn)))
            ;; d/as-of T1 has 2.0
            (is (= 2.0 (dh/q '[:find ?sf . :where [?e :sec/ticker "AAPL"] [?e :sec/split-factor ?sf]]
                             (dh/as-of @conn t1-inst))))
            ;; d/history sees all 3 historical values (1.0, 2.0, 4.0)
            (let [hist (dh/q '[:find ?sf ?added
                               :where
                               [?e :sec/ticker "AAPL"]
                               [?e :sec/split-factor ?sf _ ?added]]
                             (dh/history @conn))]
              (is (some #{[1.0 true]} hist))
              (is (some #{[2.0 true]} hist))
              (is (some #{[4.0 true]} hist)))
            (data/record-result! :datahike :bitemporal "Tx-time (as-of/history)")))

        (dh/release conn)

        (testing "Disk durability across reopen"
          (let [conn2 (dh/connect cfg)
                cnt2 (dh/q '[:find (count ?e) . :where [?e :sec/ticker _]] @conn2)]
            (is (= data/num-securities cnt2))
            (data/record-result! :datahike :durable true)
            (dh/release conn2))))
      (finally
        (try (dh/delete-database cfg) (catch Exception _))
        (data/delete-recursively! dir)))))

;; -----------------------------------------------------------------------------
;; 3. XTDB v2: Native 2D Bitemporality (valid_time + system_time) + SQL
;; -----------------------------------------------------------------------------
(deftest xtdb-v2-refdata-test
  (let [dir (data/temp-dir "xtdb2-refdata")
        node-cfg {:log [:local {:path (str dir "/log")}]
                  :storage [:local {:path (str dir "/storage")}]}
        xt-docs (mapv (fn [s]
                        {:xt/id        (:sec/id s)
                         :ticker       (:sec/ticker s)
                         :isin         (:sec/isin s)
                         :cusip        (:sec/cusip s)
                         :name         (:sec/name s)
                         :exchange     (:sec/exchange s)
                         :currency     (:sec/currency s)
                         :lot_size     (:sec/lot-size s)
                         :tick_size    (:sec/tick-size s)
                         :split_factor (:sec/split-factor s)
                         :status       (:sec/status s)})
                      securities)]
    (try
      (with-open [node (xtn/start-node node-cfg)]
        (testing "SOD batch ingestion (document-oriented :put-docs)"
          (let [t0 (data/now-ns)
                sod-valid-from (Instant/parse "2026-01-01T00:00:00Z")
                _  (xt/execute-tx node [(into [:put-docs {:into :securities :valid-from sod-valid-from}] xt-docs)])
                ms (/ (- (data/now-ns) t0) 1e6)
                cnt (-> (xt/q node ["SELECT COUNT(*) AS cnt FROM securities"])
                        first
                        :cnt)]
            (is (= data/num-securities cnt))
            (data/record-result! :xtdb-v2 :ingest-ms ms)
            (data/record-result! :xtdb-v2 :ingest-rate (/ data/num-securities (/ ms 1000.0)))))

        (testing "Intraday point-read latency (SQL)"
          (let [stats (data/bench-reads
                       sample-tickers
                       (fn [ticker]
                         (first (xt/q node ["SELECT isin, lot_size, split_factor FROM securities WHERE ticker = ?" ticker]))))]
            (data/record-result! :xtdb-v2 :reads stats)))

        (testing "Native 2D Bitemporality (VALID_TIME + SYSTEM_TIME) for corporate action correction"
          (let [aapl-base (first xt-docs)
                eff-from  (Instant/parse "2026-10-01T00:00:00Z")
                ;; T1: Vendor reports erroneous 2-for-1 split effective 2026-10-01
                tx1 (xt/execute-tx node [[:put-docs {:into :securities :valid-from eff-from}
                                          (assoc aapl-base :split_factor 2.0 :lot_size 200)]])
                sys-t1 (:system-time tx1)
                _   (Thread/sleep 10)
                ;; T2: Vendor correction arrives: 4-for-1 split effective 2026-10-01
                tx2 (xt/execute-tx node [[:put-docs {:into :securities :valid-from eff-from}
                                          (assoc aapl-base :split_factor 4.0 :lot_size 400)]])
                sys-t2 (:system-time tx2)]
            ;; Query 1: What did we believe at system_time = T1 for effective date 2026-10-01? -> 2.0
            (let [res-t1 (first (xt/q node ["SELECT split_factor, lot_size FROM securities FOR VALID_TIME AS OF TIMESTAMP '2026-10-01T12:00:00Z' FOR SYSTEM_TIME AS OF ? WHERE ticker = 'AAPL'"
                                            sys-t1]))]
              (is (= 2.0 (:split-factor res-t1)))
              (is (= 200 (:lot-size res-t1))))
            ;; Query 2: What do we know at system_time = T2 for effective date 2026-10-01? -> 4.0
            (let [res-t2 (first (xt/q node ["SELECT split_factor, lot_size FROM securities FOR VALID_TIME AS OF TIMESTAMP '2026-10-01T12:00:00Z' FOR SYSTEM_TIME AS OF ? WHERE ticker = 'AAPL'"
                                            sys-t2]))]
              (is (= 4.0 (:split-factor res-t2)))
              (is (= 400 (:lot-size res-t2))))
            ;; Query 3: Before effective date (2026-09-30), split_factor is still 1.0!
            (let [res-pre (first (xt/q node ["SELECT split_factor FROM securities FOR VALID_TIME AS OF TIMESTAMP '2026-09-30T12:00:00Z' WHERE ticker = 'AAPL'"]))]
              (is (= 1.0 (:split-factor res-pre))))
            (data/record-result! :xtdb-v2 :bitemporal "Native 2D (SQL)"))))

      (testing "Disk durability across node restart"
        (with-open [node2 (xtn/start-node node-cfg)]
          ;; Local log replays asynchronously on node startup; wait briefly for catch-up
          (let [cnt2 (loop [attempts 40]
                       (let [c (-> (xt/q node2 ["SELECT COUNT(*) AS cnt FROM securities"])
                                   first
                                   :cnt)]
                         (if (or (pos? c) (zero? attempts))
                           c
                           (do (Thread/sleep 50)
                               (recur (dec attempts))))))]
            (is (= data/num-securities cnt2))
            (data/record-result! :xtdb-v2 :durable true))))
      (finally
        (data/delete-recursively! dir)))))

;; -----------------------------------------------------------------------------
;; 4. Datomic Local: Proven immutability, d/as-of, d/history, peer-style queries
;; -----------------------------------------------------------------------------
(deftest datomic-local-refdata-test
  (let [dir (data/temp-dir "datomic-refdata")
        client (datomic/client {:server-type :datomic-local
                                :system "refdata"
                                :storage-dir dir})]
    (try
      (datomic/create-database client {:db-name "sec-master"})
      (let [conn (datomic/connect client {:db-name "sec-master"})]
        (datomic/transact conn {:tx-data datomic-schema})

        (testing "SOD batch ingestion"
          (let [t0 (data/now-ns)
                _  (doseq [chunk (partition-all 500 securities)]
                     (datomic/transact conn {:tx-data (vec chunk)}))
                ms (/ (- (data/now-ns) t0) 1e6)
                cnt (ffirst (datomic/q '[:find (count ?e) :where [?e :sec/ticker _]] (datomic/db conn)))]
            (is (= data/num-securities cnt))
            (data/record-result! :datomic-local :ingest-ms ms)
            (data/record-result! :datomic-local :ingest-rate (/ data/num-securities (/ ms 1000.0)))))

        (testing "Intraday point-read latency"
          (let [stats (data/bench-reads
                       sample-tickers
                       (fn [ticker]
                         (let [db (datomic/db conn)]
                           (first (datomic/q '[:find ?isin ?lot ?sf
                                               :in $ ?t
                                               :where
                                               [?e :sec/ticker ?t]
                                               [?e :sec/isin ?isin]
                                               [?e :sec/lot-size ?lot]
                                               [?e :sec/split-factor ?sf]]
                                             db ticker)))))]
            (data/record-result! :datomic-local :reads stats)))

        (testing "Immutable transaction-time d/as-of and d/history"
          (let [tx1 (datomic/transact conn {:tx-data [{:sec/ticker "AAPL" :sec/split-factor 2.0}]})
                t1  (get-in tx1 [:db-after :t])
                _   (datomic/transact conn {:tx-data [{:sec/ticker "AAPL" :sec/split-factor 4.0}]})
                db-now (datomic/db conn)]
            (is (= 4.0 (ffirst (datomic/q '[:find ?sf :where [?e :sec/ticker "AAPL"] [?e :sec/split-factor ?sf]] db-now))))
            (is (= 2.0 (ffirst (datomic/q '[:find ?sf :where [?e :sec/ticker "AAPL"] [?e :sec/split-factor ?sf]]
                                          (datomic/as-of db-now t1)))))
            (let [hist (set (datomic/q '[:find ?sf ?added
                                         :where
                                         [?e :sec/ticker "AAPL"]
                                         [?e :sec/split-factor ?sf _ ?added]]
                                       (datomic/history db-now)))]
              (is (contains? hist [1.0 true]))
              (is (contains? hist [2.0 true]))
              (is (contains? hist [4.0 true])))
            (data/record-result! :datomic-local :bitemporal "Tx-time (as-of/history)"))))

      (testing "Disk durability across fresh client reconnect"
        (let [client2 (datomic/client {:server-type :datomic-local
                                        :system "refdata"
                                        :storage-dir dir})
              conn2 (datomic/connect client2 {:db-name "sec-master"})
              cnt2 (ffirst (datomic/q '[:find (count ?e) :where [?e :sec/ticker _]] (datomic/db conn2)))]
          (is (= data/num-securities cnt2))
          (data/record-result! :datomic-local :durable true)))
      (finally
        (data/delete-recursively! dir)))))

;; -----------------------------------------------------------------------------
;; 5. DataScript: Ephemeral in-memory cache only (no durability, no history)
;; -----------------------------------------------------------------------------
(deftest datascript-refdata-test
  (let [conn (ds/create-conn datascript-schema)]
    (testing "SOD batch ingestion (in-memory)"
      (let [t0 (data/now-ns)
            _  (ds/transact! conn securities)
            ms (/ (- (data/now-ns) t0) 1e6)
            cnt (ds/q '[:find (count ?e) . :where [?e :sec/ticker _]] @conn)]
        (is (= data/num-securities cnt))
        (data/record-result! :datascript :ingest-ms ms)
        (data/record-result! :datascript :ingest-rate (/ data/num-securities (/ ms 1000.0)))))

    (testing "Intraday point-read latency (in-memory)"
      (let [stats (data/bench-reads
                   sample-tickers
                   (fn [ticker]
                     (ds/q '[:find [?isin ?lot ?sf]
                             :in $ ?t
                             :where
                             [?e :sec/ticker ?t]
                             [?e :sec/isin ?isin]
                             [?e :sec/lot-size ?lot]
                             [?e :sec/split-factor ?sf]]
                           @conn ticker)))]
        (data/record-result! :datascript :reads stats)))

    (testing "Mutable in-memory only: no history/as-of and no persistence"
      (ds/transact! conn [{:sec/ticker "AAPL" :sec/split-factor 4.0}])
      (is (= 4.0 (ds/q '[:find ?sf . :where [?e :sec/ticker "AAPL"] [?e :sec/split-factor ?sf]] @conn)))
      (is (nil? (resolve 'datascript.core/as-of)))
      (is (nil? (resolve 'datascript.core/history)))
      ;; New conn has no persisted state
      (let [fresh-conn (ds/create-conn datascript-schema)]
        (is (nil? (ds/q '[:find (count ?e) . :where [?e :sec/ticker _]] @fresh-conn))))
      (data/record-result! :datascript :bitemporal "None (mutable mem)")
      (data/record-result! :datascript :durable false))))
