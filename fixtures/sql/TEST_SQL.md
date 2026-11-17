# SQL code blocks

A valid query:

```sql
SELECT o.id, c.name
FROM main.sales.orders AS o
JOIN main.sales.customers AS c ON o.customer_id = c.id
```

A query with a typo:

```sql
SELECT id
FORM main.sales.orders
```

A table which does not exist:

```sql
SELECT * FROM main.sales.refunds
```

A catalog which does not exist:

```sql
SELECT * FROM legacy.sales.orders
```

Table names are qualified with `USE` statements. Common table expressions are
not tables:

```sql
USE CATALOG main;
USE SCHEMA sales;
WITH recent AS (SELECT * FROM orders WHERE order_date > current_date() - 7)
SELECT * FROM recent
```

Code blocks can be skipped:

```sql ignore
SELECT * FROM {{ table }}
```

Other languages are not checked:

```python
spark.sql("PICK 1")
```
