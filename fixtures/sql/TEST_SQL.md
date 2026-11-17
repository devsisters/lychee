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

Code blocks can be skipped:

```sql ignore
SELECT * FROM {{ table }}
```

Other languages are not checked:

```python
spark.sql("SELEC 1")
```
